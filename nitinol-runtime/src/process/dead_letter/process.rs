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

impl DeadLetterEnvelope {
    /// The envelope carrying `msg`, undeliverable to `destination`.
    ///
    /// The type id is read off `M` here rather than taken from the caller: it
    /// is what the throttle groups by and what the watch path recognises a
    /// `WatchRequest` by, so an envelope naming a type other than the one it
    /// boxed would throttle the wrong messages and hide a termination.  A
    /// caller has nothing to state but the message itself.
    pub(crate) fn new<M>(msg: M, destination: Pid, sender: Option<Pid>, suppress_log: bool) -> Self
    where
        M: 'static + Send + Sync,
    {
        Self {
            destination,
            message: BoxedMessage::new(msg),
            sender,
            suppress_log,
            message_type_id: TypeId::of::<M>(),
        }
    }
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
        // Ignore send error: if the process is stopped, dead-letter routing silently
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

#[cfg(test)]
mod tests {
    use super::*;

    struct Ping;

    struct Pong;

    /// Given a message, When an envelope is built for it, Then the type the
    /// envelope names is the type it boxed.
    ///
    /// The throttle groups undeliverable messages by that type id, and the
    /// watch path recognises a `WatchRequest` by it, so an envelope naming
    /// anything else would throttle the wrong messages and leave a watcher
    /// waiting for a termination it was never told about.
    #[test]
    fn an_envelope_names_the_type_of_the_message_it_carries() {
        let envelope = DeadLetterEnvelope::new(Ping, Pid::next(), None, false);

        assert_eq!(
            envelope.message_type_id,
            TypeId::of::<Ping>(),
            "the envelope must name the type of the message it was built from"
        );
        assert!(
            envelope.message.downcast_ref::<Ping>().is_some(),
            "the boxed message must still be readable as the type the envelope names"
        );
        assert!(
            envelope.message.downcast_ref::<Pong>().is_none(),
            "the boxed message must not be readable as any other type"
        );
    }

    /// Two messages of different types are told apart, which is what makes the
    /// type id usable as the throttle's key.
    #[test]
    fn envelopes_for_different_message_types_are_named_apart() {
        let destination = Pid::next();

        let ping = DeadLetterEnvelope::new(Ping, destination, None, false);
        let pong = DeadLetterEnvelope::new(Pong, destination, None, false);

        assert_ne!(
            ping.message_type_id, pong.message_type_id,
            "messages of different types must not share one throttle key"
        );
    }
}
