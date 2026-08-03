use nitinol_runtime::error::HandlerError;
use nitinol_runtime::process::{Driver, Process, ProcessContext, ProcessProxy};
use nitinol_runtime::{IdleTimeout, Props};

use crate::effect::TellIntent;
use crate::outbox::{RetryPolicy, TellOutcome};
use crate::process::saga_process::{OutboxReport, SagaProcess};
use crate::saga::Saga;

pub(crate) struct OutboxExecutorProcess<S: Saga> {
    intent: TellIntent,
    policy: RetryPolicy,
    tell_id: u64,
    saga_proxy: ProcessProxy<SagaProcess<S>>,
}

impl<S: Saga> Process for OutboxExecutorProcess<S> {}

impl<S: Saga> OutboxExecutorProcess<S> {
    async fn settle(&self, ctx: &mut ProcessContext<Self>, outcome: TellOutcome) {
        if let Err(e) = self
            .saga_proxy
            .tell(OutboxReport {
                tell_id: self.tell_id,
                outcome,
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
        if attempt > state.policy.max_attempts {
            self.finished = true;
            state.settle(ctx, TellOutcome::Failed).await;
            return Ok(());
        }
        match state.intent.side.execute_once().await {
            Ok(()) => {
                self.finished = true;
                state.settle(ctx, TellOutcome::Acked).await;
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
                    state.settle(ctx, TellOutcome::Failed).await;
                }
            }
        }
        Ok(())
    }

    fn supports_idle_timeout(&self) -> bool {
        false
    }
}

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
