use std::any::TypeId;
use std::collections::HashMap;
use std::marker::PhantomData;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use crate::error::BoxError;
use crate::ident::Pid;
use crate::process::message::Boxed;
use crate::process::task::{TellTask, UserTask};
use crate::process::{Process, ProcessContext, ProcessProxy, Receive};
use crate::process::stream::Stream;

// -- Public marker trait -------------------------------------------------------

/// Implementing this trait suppresses dead-letter log output for the type.
/// Stream notification is NOT suppressed — messages still reach the stream.
pub trait SuppressDeadLetterLog {}

// -- Autoref specialization: stable-Rust way to detect the marker at call sites --

struct SuppressProbe<T>(PhantomData<T>);

// High priority: inherent method, only compiled when T: SuppressDeadLetterLog
impl<T: SuppressDeadLetterLog> SuppressProbe<T> {
    // Called via method resolution (inherent over trait), not by name — suppress false positive.
    #[allow(dead_code)]
    fn value(&self) -> bool {
        true
    }
}

// Low priority: trait method, always available
trait FallbackValue {
    fn value(&self) -> bool;
}

impl<T> FallbackValue for SuppressProbe<T> {
    fn value(&self) -> bool {
        false
    }
}

/// Returns `true` when type `M` implements `SuppressDeadLetterLog`.
/// Must be called at a monomorphisation site where `M` is concrete.
pub(crate) fn suppress_log<M>() -> bool {
    (&SuppressProbe::<M>(PhantomData)).value()
}

// -- Public domain types ------------------------------------------------------

/// A message that could not be delivered to its intended recipient.
pub struct DeadLetter {
    pub destination: Pid,
    pub message: Boxed,
    pub sender: Option<Pid>,
}

/// Error returned to the caller of `ask` when the target process is unreachable.
#[derive(Debug)]
pub struct DeadLetterResponse {
    pub destination: Pid,
}

impl std::fmt::Display for DeadLetterResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "dead letter: no process at pid {}", self.destination)
    }
}

impl std::error::Error for DeadLetterResponse {}

// -- Internal envelope --------------------------------------------------------

pub(crate) struct DeadLetterEnvelope {
    pub(crate) destination: Pid,
    pub(crate) message: Boxed,
    pub(crate) sender: Option<Pid>,
    pub(crate) suppress_log: bool,
    pub(crate) message_type_id: TypeId,
}

// -- DeadLetterRef (internal send handle) -------------------------------------

/// Cloneable handle for routing undeliverable messages to `DeadLetterActor`.
#[derive(Clone)]
pub(crate) struct DeadLetterRef {
    tx: mpsc::Sender<UserTask<DeadLetterActor>>,
}

impl DeadLetterRef {
    pub(crate) fn new(tx: mpsc::Sender<UserTask<DeadLetterActor>>) -> Self {
        Self { tx }
    }

    pub(crate) async fn send(&self, envelope: DeadLetterEnvelope) {
        let task: UserTask<DeadLetterActor> =
            Box::new(TellTask::new(DeadLetterEnvelopeMsg(envelope)));
        // Ignore send error: if the actor is stopped, dead-letter routing silently
        // drops the envelope rather than blocking the caller.
        let _ = self.tx.send(task).await;
    }
}

// -- DeadLetterActor ----------------------------------------------------------

struct DeadLetterEnvelopeMsg(DeadLetterEnvelope);

pub(crate) struct DeadLetterActor {
    stream: ProcessProxy<Stream<Boxed>>,
    throttle: LogThrottle,
}

impl DeadLetterActor {
    pub(crate) fn new(stream: ProcessProxy<Stream<Boxed>>) -> Self {
        Self {
            stream,
            throttle: LogThrottle::new(),
        }
    }
}

impl Process for DeadLetterActor {}

impl Receive<DeadLetterEnvelopeMsg> for DeadLetterActor {
    type Response = ();

    async fn recv(
        &mut self,
        msg: DeadLetterEnvelopeMsg,
        _ctx: &mut ProcessContext,
    ) -> Result<(), BoxError> {
        let envelope = msg.0;
        let suppress =
            envelope.suppress_log || self.throttle.is_throttled(envelope.message_type_id);
        if !suppress {
            tracing::warn!(
                destination = %envelope.destination,
                "dead letter: message undeliverable to process {}",
                envelope.destination,
            );
        }
        let dead_letter = DeadLetter {
            destination: envelope.destination,
            message: envelope.message,
            sender: envelope.sender,
        };
        let _ = self.stream.publish(dead_letter).await;
        Ok(())
    }
}

// -- LogThrottle --------------------------------------------------------------

const THROTTLE_WINDOW: Duration = Duration::from_secs(10);
const THROTTLE_LIMIT: u32 = 10;

struct WindowCounter {
    count: u32,
    window_start: Instant,
}

struct LogThrottle {
    counters: HashMap<TypeId, WindowCounter>,
}

impl LogThrottle {
    fn new() -> Self {
        Self {
            counters: HashMap::new(),
        }
    }

    /// Returns `true` if further log output for this type should be suppressed.
    /// Increments the counter; resets the window when expired.
    fn is_throttled(&mut self, type_id: TypeId) -> bool {
        let now = Instant::now();
        let counter = self.counters.entry(type_id).or_insert_with(|| WindowCounter {
            count: 0,
            window_start: now,
        });
        if now.duration_since(counter.window_start) >= THROTTLE_WINDOW {
            counter.count = 0;
            counter.window_start = now;
        }
        counter.count += 1;
        counter.count > THROTTLE_LIMIT
    }
}
