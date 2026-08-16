//! `EnqueuePolicy` — the per-saga filter deciding which failures reach the DLQ.

use std::sync::Arc;

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
/// The default (`EnqueueAll`) enqueues every failure kind; an implementor can
/// override [`decide`](EnqueuePolicy::decide) to suppress selected (or all)
/// failures.  Wired via [`crate::SagaProps::with_enqueue_policy`] for a
/// standalone saga, or [`crate::SagaManagerProps::with_enqueue_policy`] for
/// every instance a manager spawns.
pub trait EnqueuePolicy: Send + Sync {
    fn decide(&self, failure: &SagaFailure) -> EnqueueDecision;
}

/// The default policy: enqueue every failure kind.
pub(crate) struct EnqueueAll;

impl EnqueuePolicy for EnqueueAll {
    fn decide(&self, _failure: &SagaFailure) -> EnqueueDecision {
        EnqueueDecision::Enqueue
    }
}

/// The policy an instance runs with when its spawn boundary was given none.
///
/// Owned here so every spawn path — the standalone builder and the manager
/// that spawns instances per correlation id — resolves the same default
/// instead of each picking its own.
pub(crate) fn default_enqueue_policy() -> Arc<dyn EnqueuePolicy> {
    Arc::new(EnqueueAll)
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
