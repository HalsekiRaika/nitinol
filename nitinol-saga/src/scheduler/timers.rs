//! `Timers` — the per-saga view over the shared [`SchedulerProxy`].
//!
//! Binds a saga's [`SagaId`] to the resident scheduler so the effect
//! interpreter and lifecycle hooks name their timer operations
//! (`start_single` / `cancel` / `cancel_all`) instead of assembling
//! [`ScheduleToken`]s at each call site.

use std::time::Duration;

use crate::id::SagaId;
use crate::scheduler::process::{DispatchFn, SchedulerProxy};
use crate::scheduler::token::{ScheduleToken, TimerName};

pub(crate) struct Timers {
    owner: SagaId,
    scheduler: SchedulerProxy,
}

impl Timers {
    pub(crate) fn new(owner: SagaId, scheduler: SchedulerProxy) -> Self {
        Self { owner, scheduler }
    }

    fn token(&self, name: TimerName) -> ScheduleToken {
        ScheduleToken {
            saga_id: self.owner.clone(),
            name,
        }
    }

    /// Register a single-shot timer under `name`, replacing any earlier timer
    /// with the same name (`startSingleTimer` semantics).
    pub(crate) async fn start_single(
        &self,
        name: TimerName,
        after: Duration,
        dispatch: DispatchFn,
    ) {
        self.scheduler
            .register(self.token(name), after, dispatch)
            .await;
    }

    /// Cancel the timer registered under `name`, if any.
    pub(crate) async fn cancel(&self, name: TimerName) {
        self.scheduler.cancel(self.token(name)).await;
    }

    /// Cancel every timer this saga owns (invoked on saga stop).
    pub(crate) async fn cancel_all(&self) {
        self.scheduler.cancel_all(self.owner.clone()).await;
    }
}
