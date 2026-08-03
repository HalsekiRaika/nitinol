//! `EnqueuePolicy` — the per-saga filter deciding which failures reach the DLQ
//! (Issue #51, G-26).

use crate::dead_letter::event::SagaFailure;

/// Whether a given [`SagaFailure`] should be enqueued as a dead letter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnqueueDecision {
    /// Persist the failure as a dead letter on the saga stream.
    Enqueue,
    /// Suppress the failure — no dead letter is written.
    Ignore,
}

/// A user-supplied filter over saga failures.
///
/// The default ([`EnqueueAll`]) enqueues every failure kind; an implementor can
/// override [`decide`](EnqueuePolicy::decide) to suppress selected (or all)
/// failures.  Wired via [`crate::SagaProps::with_enqueue_policy`].
pub trait EnqueuePolicy: Send + Sync {
    fn decide(&self, failure: &SagaFailure) -> EnqueueDecision;
}

/// The default policy: enqueue every failure kind (G-26).
pub(crate) struct EnqueueAll;

impl EnqueuePolicy for EnqueueAll {
    fn decide(&self, _failure: &SagaFailure) -> EnqueueDecision {
        EnqueueDecision::Enqueue
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    #[test]
    fn enqueue_all_enqueues_every_failure() {
        let policy = EnqueueAll;
        let failures = [
            SagaFailure::HandleFailed {
                error: "e".to_owned(),
            },
            SagaFailure::TellFailed {
                target: crate::SagaId::new(""),
                message: Bytes::new(),
            },
        ];
        for failure in failures {
            assert_eq!(policy.decide(&failure), EnqueueDecision::Enqueue);
        }
    }
}
