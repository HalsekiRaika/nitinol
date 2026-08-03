use std::any::TypeId;

use tokio::sync::mpsc;

use super::throttle::LogThrottle;
use super::DeadLetter;
use crate::ident::Pid;
use crate::process::message::BoxedMessage;
use crate::process::registry::ProcessRegistry;
use crate::process::signal::SystemSignal;
use crate::process::stream::Stream;
use crate::process::task::{TellTask, UserTask};
use crate::process::watch::{TerminatedReason, WatchRequest};
use crate::process::{Process, ProcessContext, ProcessProxy, Receive};

pub(crate) struct DeadLetterEnvelope {
    pub(crate) destination: Pid,
    pub(crate) message: BoxedMessage,
    pub(crate) sender: Option<Pid>,
    pub(crate) suppress_log: bool,
    pub(crate) message_type_id: TypeId,
}

/// Cloneable handle for routing undeliverable messages to `DeadLetterProcess`.
#[derive(Clone)]
pub(crate) struct DeadLetterProxy {
    tx: mpsc::Sender<UserTask<DeadLetterProcess>>,
}

impl DeadLetterProxy {
    pub(crate) fn new(tx: mpsc::Sender<UserTask<DeadLetterProcess>>) -> Self {
        Self { tx }
    }

    pub(crate) async fn send(&self, envelope: DeadLetterEnvelope) {
        let task: UserTask<DeadLetterProcess> =
            Box::new(TellTask::new(DeadLetterEnvelopeMsg(envelope)));
        // Ignore send error: if the actor is stopped, dead-letter routing silently
        // drops the envelope rather than blocking the caller.
        let _ = self.tx.send(task).await;
    }
}

struct DeadLetterEnvelopeMsg(DeadLetterEnvelope);

pub(crate) struct DeadLetterProcess {
    stream: ProcessProxy<Stream<BoxedMessage>>,
    throttle: LogThrottle,
    registry: ProcessRegistry,
}

impl DeadLetterProcess {
    pub(crate) fn new(
        stream: ProcessProxy<Stream<BoxedMessage>>,
        registry: ProcessRegistry,
    ) -> Self {
        Self {
            stream,
            throttle: LogThrottle::new(),
            registry,
        }
    }
}

impl Process for DeadLetterProcess {}

impl Receive<DeadLetterEnvelopeMsg> for DeadLetterProcess {
    type Response = ();
    type Error = std::convert::Infallible;

    async fn recv(
        &mut self,
        msg: DeadLetterEnvelopeMsg,
        _ctx: &mut ProcessContext<Self>,
    ) -> Result<(), std::convert::Infallible> {
        let envelope = msg.0;
        let suppress =
            envelope.suppress_log || self.throttle.check_throttle(envelope.message_type_id);
        if !suppress {
            tracing::warn!(
                destination = %envelope.destination,
                "dead letter: message undeliverable to process {}",
                envelope.destination,
            );
        }

        // Detect WatchRequest before consuming the envelope message.
        // Extract Copy PIDs to avoid holding the downcast ref across await points.
        let watch_pair: Option<(Pid, Pid)> =
            if envelope.message_type_id == TypeId::of::<WatchRequest>() {
                envelope
                    .message
                    .downcast_ref::<WatchRequest>()
                    .map(|req| (req.watched, req.watcher))
            } else {
                None
            };

        let dead_letter = DeadLetter {
            destination: envelope.destination,
            message: envelope.message,
            sender: envelope.sender,
        };
        let _ = self.stream.publish_boxed(dead_letter).await;

        if let Some((watched, watcher)) = watch_pair {
            if let Some(proxy) = self.registry.lookup(watcher).await {
                let _ = proxy
                    .send_system_signal(SystemSignal::Terminated {
                        who: watched,
                        why: TerminatedReason::NotFound,
                    })
                    .await;
            }
        }

        Ok(())
    }
}
