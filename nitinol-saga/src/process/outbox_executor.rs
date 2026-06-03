//! Per-tell retry executor.
//!
//! Spawned by the interpreter after a Persist batch with tells is durably
//! appended.  Owns the [`TellIntent`] for as long as the retry loop runs.
//! Re-invokes the typed side effect up to [`RetryPolicy::max_attempts`] times
//! with exponential backoff.  On success appends a `TellAcked` outbox marker;
//! on exhaustion appends a `TellFailed` marker.
//!
//! When the terminal marker is appended **successfully**, the executor sends
//! [`OutboxTerminalSettled`] back to the saga's own mailbox via `self_proxy`.
//! The saga's handler removes the entry from the in-memory `pending_intents`
//! registry and, when `failed == true`, records the `tell_id` so that the next
//! [`Saga::handle`] invocation can observe the failure.  If `append_terminal`
//! fails the executor sends [`OutboxTerminalAppendFailed`] instead: the saga
//! decrements its `pending_executor_count` without removing the
//! `pending_intents` entry, so the intent survives for supervised-restart
//! re-dispatch while a deferred `End` can still complete.

use nitinol_runtime::process::ProcessProxy;

use crate::effect::TellIntent;
use crate::outbox::{RetryPolicy, TerminalKind};
use crate::process::saga_process::{
    AppendTerminalAndClaim, OutboxTerminalAppendFailed, OutboxTerminalSettled, SagaProcess,
};
use crate::saga::Saga;

pub(crate) fn spawn_outbox_executor<S: Saga>(
    intent: TellIntent,
    tell_id: u64,
    policy: RetryPolicy,
    saga_proxy: ProcessProxy<SagaProcess<S>>,
) {
    tokio::spawn(async move {
        let succeeded = run_attempts(&intent, &policy, tell_id).await;
        let kind = if succeeded {
            TerminalKind::Acked
        } else {
            TerminalKind::Failed
        };
        // Ask the saga's own loop to append the terminal marker and advance the
        // sequence cursor atomically.  The sequence only advances on a
        // successful store call, so a failure here never skips a sequence number.
        let appended = match saga_proxy
            .ask(AppendTerminalAndClaim { tell_id, kind })
            .await
        {
            Ok(result) => result,
            Err(e) => {
                tracing::warn!(
                    error = ?e,
                    tell_id,
                    "saga outbox executor failed to append terminal; saga process may be stopping"
                );
                return;
            }
        };
        if appended {
            // Tell the saga to remove the entry from `pending_intents`.  If the
            // append failed we deliberately skip this notification so the
            // entry survives and a subsequent supervised restart can still
            // re-dispatch the tell.
            let failed = !succeeded;
            if let Err(e) = saga_proxy
                .tell(OutboxTerminalSettled { tell_id, failed })
                .await
            {
                tracing::warn!(
                    error = ?e,
                    tell_id,
                    "saga outbox executor failed to notify saga of settled terminal"
                );
            }
        } else {
            // Terminal append failed.  Notify the saga so it can decrement
            // `pending_executor_count` and complete a deferred End if one is
            // pending.  The `pending_intents` entry is deliberately preserved
            // (handled inside `OutboxTerminalAppendFailed`) so a subsequent
            // supervised restart can re-dispatch the tell.
            if let Err(e) = saga_proxy
                .tell(OutboxTerminalAppendFailed { tell_id })
                .await
            {
                tracing::warn!(
                    error = ?e,
                    tell_id,
                    "saga outbox executor failed to notify saga of terminal append failure"
                );
            }
        }
    });
}

async fn run_attempts(intent: &TellIntent, policy: &RetryPolicy, tell_id: u64) -> bool {
    for attempt in 1..=policy.max_attempts {
        if attempt > 1 {
            tokio::time::sleep(policy.backoff_before(attempt)).await;
        }
        match intent.side.execute_once().await {
            Ok(()) => return true,
            Err(e) => {
                tracing::debug!(
                    error = %e,
                    attempt,
                    tell_id,
                    "saga tell attempt failed; will retry if attempts remain"
                );
            }
        }
    }
    tracing::warn!(
        max_attempts = policy.max_attempts,
        tell_id,
        "saga tell exhausted retries; appending TellFailed"
    );
    false
}
