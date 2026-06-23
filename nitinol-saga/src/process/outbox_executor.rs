//! Per-tell retry executor — a short-lived **child process** of the saga.
//!
//! Spawned by the interpreter (after a Persist batch with tells is durably
//! appended) and by the replay path (supervised-restart / crash-restart
//! re-dispatch) via [`ProcessContext::spawn_child`], so the executor lives on
//! the saga's supervision tree and cascade-stops when the saga stops.
//!
//! The executor owns the [`TellIntent`] and re-invokes its typed side effect up
//! to [`RetryPolicy::max_attempts`] times with exponential backoff. The retry
//! loop is implemented as a custom [`Driver`] ([`OutboxRetryDriver`]): the
//! backoff wait lives in `next()` so the lifecycle loop multiplexes it (biased
//! `select!`) with the system-signal receiver. A `Stop` cascaded from the
//! parent therefore interrupts the executor **between attempts** instead of
//! letting it run the retry budget to completion.
//!
//! On success the executor appends a `TellAcked` outbox marker; on retry
//! exhaustion it appends a `TellFailed` marker. In both cases it sends
//! [`AppendTerminalAndClaim`] to the parent saga (which performs the durable
//! append and the in-memory cleanup atomically inside the saga's own loop) and
//! then `stop_self`. The executor defines no inbound messages — it is a
//! self-driving process; its only control input is the `Stop` system signal.

use nitinol_runtime::error::HandlerError;
use nitinol_runtime::process::{Driver, Process, ProcessContext, ProcessProxy};
use nitinol_runtime::{IdleTimeout, Props};

use crate::effect::TellIntent;
use crate::outbox::{RetryPolicy, TerminalKind};
use crate::process::saga_process::{AppendTerminalAndClaim, SagaProcess};
use crate::saga::Saga;

/// Short-lived child process that runs the retry loop for a single outbox tell.
///
/// `saga_proxy` is the typed handle to the parent saga; the executor cannot
/// obtain it from its own `ctx` (that proxy is typed to `OutboxExecutorProcess`),
/// so the parent injects it at spawn time. It is used to send
/// [`AppendTerminalAndClaim`] back to the saga's own loop.
pub(crate) struct OutboxExecutorProcess<S: Saga> {
    intent: TellIntent,
    policy: RetryPolicy,
    tell_id: u64,
    saga_proxy: ProcessProxy<SagaProcess<S>>,
}

impl<S: Saga> Process for OutboxExecutorProcess<S> {}

impl<S: Saga> OutboxExecutorProcess<S> {
    /// Send the terminal claim to the saga, then stop this child.
    ///
    /// Fire-and-forget `tell`: the saga appends the marker and advances its
    /// sequence cursor inside its own single-threaded loop, so the executor
    /// does not need to wait for the durable result before terminating.
    async fn settle(&self, ctx: &mut ProcessContext<Self>, kind: TerminalKind) {
        if let Err(e) = self
            .saga_proxy
            .tell(AppendTerminalAndClaim {
                tell_id: self.tell_id,
                kind,
            })
            .await
        {
            tracing::warn!(
                error = ?e,
                tell_id = self.tell_id,
                "saga outbox executor failed to send terminal claim; saga process may be stopping"
            );
        }
        if let Err(e) = ctx.stop_self().await {
            tracing::warn!(
                error = %e,
                tell_id = self.tell_id,
                "saga outbox executor stop_self failed"
            );
        }
    }
}

/// Custom driver implementing the per-attempt backoff-then-try loop.
///
/// `next()` produces the upcoming 1-based attempt number after waiting the
/// backoff for that attempt; the wait runs here (not in `apply`) so it is
/// cancellable by a `Stop` signal via the lifecycle `select!`. `apply()`
/// performs exactly one side-effect attempt and, on a terminal outcome, marks
/// the driver `finished` and settles the process.
pub(crate) struct OutboxRetryDriver {
    policy: RetryPolicy,
    attempt: usize,
    finished: bool,
}

impl OutboxRetryDriver {
    pub(crate) fn new(policy: RetryPolicy) -> Self {
        Self {
            policy,
            attempt: 0,
            finished: false,
        }
    }
}

impl<S: Saga> Driver<OutboxExecutorProcess<S>> for OutboxRetryDriver {
    type Event = usize;

    async fn next(&mut self) -> Option<usize> {
        if self.finished {
            // Terminal outcome already settled: stay pending forever so the
            // process stops via the queued `Stop` rather than re-attempting.
            return std::future::pending().await;
        }
        self.attempt += 1;
        if self.attempt > 1 {
            tokio::time::sleep(self.policy.backoff_before(self.attempt)).await;
        }
        Some(self.attempt)
    }

    async fn apply(
        &mut self,
        state: &mut OutboxExecutorProcess<S>,
        ctx: &mut ProcessContext<OutboxExecutorProcess<S>>,
        attempt: usize,
    ) -> Result<(), HandlerError> {
        // Guard for `max_attempts == 0`: no attempt is permitted, so settle
        // Failed immediately without invoking the side effect.
        if attempt > state.policy.max_attempts {
            self.finished = true;
            state.settle(ctx, TerminalKind::Failed).await;
            return Ok(());
        }
        match state.intent.side.execute_once().await {
            Ok(()) => {
                self.finished = true;
                state.settle(ctx, TerminalKind::Acked).await;
            }
            Err(e) => {
                tracing::debug!(
                    error = %e,
                    attempt,
                    tell_id = state.tell_id,
                    "saga tell attempt failed; will retry if attempts remain"
                );
                if attempt >= state.policy.max_attempts {
                    tracing::warn!(
                        max_attempts = state.policy.max_attempts,
                        tell_id = state.tell_id,
                        "saga tell exhausted retries; appending TellFailed"
                    );
                    self.finished = true;
                    state.settle(ctx, TerminalKind::Failed).await;
                }
            }
        }
        Ok(())
    }

    fn supports_idle_timeout(&self) -> bool {
        false
    }
}

/// Spawn an [`OutboxExecutorProcess`] as a child of the saga process.
///
/// The child is registered in the saga's supervision tree, so it cascade-stops
/// with the parent. Supervision strategy is the [`Props::new`] default
/// (`Stop`): a failed executor is not restarted. The idle timeout is disabled
/// (`IdleTimeout::Persistent`) because a self-driving process is "idle" by
/// definition between attempts.
pub(crate) async fn spawn_outbox_executor<S: Saga>(
    ctx: &mut ProcessContext<SagaProcess<S>>,
    intent: TellIntent,
    tell_id: u64,
    policy: RetryPolicy,
) {
    let saga_proxy = ctx.self_proxy().clone();
    let producer = {
        let intent = intent.clone();
        let policy = policy.clone();
        move || OutboxExecutorProcess {
            intent: intent.clone(),
            policy: policy.clone(),
            tell_id,
            saga_proxy: saga_proxy.clone(),
        }
    };
    let props = Props::new(producer)
        .with_idle_timeout(IdleTimeout::Persistent)
        .with_driver(OutboxRetryDriver::new(policy));
    ctx.spawn_child(props).await;
}
