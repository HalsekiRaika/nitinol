//! Routing a disposition marker to whoever owns the saga stream's sequence.
//!
//! A stream has exactly one writer.  For a saga's own stream that writer is the
//! resident [`SagaProcess`](crate::process::saga_process::SagaProcess), which
//! holds the next sequence in memory — so an operator settling a dead letter
//! cannot simply append beside it.  Instead the settle is *asked* of the single
//! arbiter of that stream, the `SagaManager`, and it decides where the write
//! belongs:
//!
//! | target state | who appends |
//! |---|---|
//! | resident | the instance, from its own mailbox, at its own next sequence |
//! | dormant  | the manager itself, at the stream tail it just read |
//!
//! Two message types rather than one, because the two hops answer different
//! questions.  [`SettleDeadLetter`] asks the manager *which stream, and who
//! owns it*; [`RecordDisposition`] tells an instance *append this to yourself*,
//! where the saga id is already settled by whose mailbox it arrived in.
//!
//! The operator waits for the answer.  A disposition is a recovery action, and
//! an operator who is not told whether the write landed cannot know whether to
//! retry it.

use futures_core::future::BoxFuture;
use nitinol_persistence::error::{AppendError, LoadError};

use crate::id::SagaId;

use super::disposition::DeadLetterDispositionEvent;

/// Ask the manager to settle a dead letter on `saga_id`'s stream.
///
/// Carries the finished marker: what to write was decided by the
/// [`DeadLetterQueue`](crate::DeadLetterQueue), which is the part that knows
/// what a dead letter is.  The arbiter decides only where it goes.
pub(crate) struct SettleDeadLetter {
    pub(crate) saga_id: SagaId,
    pub(crate) marker: DeadLetterDispositionEvent,
}

/// Tell a resident instance to append `marker` to its own stream.
///
/// No saga id: the instance owns exactly one stream, and the message reached it
/// by being routed to that instance.
pub(crate) struct RecordDisposition {
    pub(crate) marker: DeadLetterDispositionEvent,
}

/// Why a settle did not happen.
#[derive(Debug, thiserror::Error)]
pub(crate) enum SettleError {
    #[error(transparent)]
    Append(#[from] AppendError),
    #[error(transparent)]
    Load(#[from] LoadError),
    /// The arbiter — or the instance it routed to — could not be reached.
    ///
    /// The write did not happen and the dead letter is still outstanding, so
    /// the operator can simply run the operation again once the manager or the
    /// instance is back.  Distinct from [`SettleError::Append`], which means
    /// the store itself refused the marker.
    #[error("the stream's arbiter did not answer: {0}")]
    Unreachable(String),
}

/// The single writer of a saga's stream, as the queue sees it.
///
/// Type-erased so [`DeadLetterQueue`](crate::DeadLetterQueue) — which is not
/// generic over the saga type — can hold a handle to a manager that is.
pub(crate) trait DispositionArbiter: Send + Sync {
    fn settle<'a>(
        &'a self,
        saga_id: &'a SagaId,
        marker: DeadLetterDispositionEvent,
    ) -> BoxFuture<'a, Result<(), SettleError>>;
}
