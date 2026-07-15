use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures_core::future::BoxFuture;
use nitinol_eventsource::codec::ErasedCodec;
use nitinol_eventsource::{DurableSubscription, SequenceCursor};
use nitinol_persistence::store::EventStore;
use nitinol_persistence::AggregateId;
use nitinol_runtime::process::{Process, ProcessContext, ProcessProxy, Receive};

use crate::context::SagaContext;
use crate::dead_letter::{
    enqueue_dead_letter, DlqChildSpawn, EnqueueOutcome, EnqueuePolicy, SagaFailure, SourceContext,
};
use crate::effect::TellIntent;
use crate::id::SagaId;
use crate::outbox::{OutboxAppender, RetryPolicy, TellOutcome};
use crate::error::SagaUpstreamHandlerError;
use crate::process::interpreter::{run_saga_effect, InterpretOutcome, InterpreterCtx};
use crate::process::replay::{replay_and_redispatch, ActiveSchedule};
use crate::saga::Saga;
use crate::scheduler::{
    append_schedule_marker, DispatchFn, ScheduleEvent, SchedulerProxy, Timers,
};
use crate::scheduler::{ScheduleToken, TimerName};

/// A typed upstream event delivered to the saga by its `DirectPollerProcess`.
///
/// Either the successfully decoded event (carrying both typed and raw bytes for
/// the draining dead-letter path) or a decode failure that the transform could
/// not recover — both are forwarded to `SagaProcess` so the process can record
/// a `SagaFailure::DecodeFailed` dead letter (G-26 / G-30 immediate).
pub(crate) enum UpstreamMessage<E> {
    /// Upstream event decoded successfully.
    Decoded {
        aggregate_id: AggregateId,
        sequence: u64,
        event: E,
        /// Raw bytes as loaded from the upstream EventStore, preserved through
        /// the transform so the dead letter carries the actual payload.
        raw_payload: Bytes,
    },
    /// The upstream codec failed to decode the event payload.  Routing is
    /// impossible without a typed event, so the failure is always delivered to
    /// this saga for DLQ recording (G-26).
    DecodeFailed {
        aggregate_id: AggregateId,
        sequence: u64,
        error: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Lifecycle {
    Running,
    Draining,
}

#[cfg_attr(test, derive(Clone))]
pub(crate) enum TellState {
    Pending(TellIntent),
    AppendFailed(TellIntent),
    Failed,
}

pub(crate) fn in_flight_count(tell_states: &HashMap<u64, TellState>) -> usize {
    tell_states
        .values()
        .filter(|state| matches!(state, TellState::Pending(_)))
        .count()
}

pub(crate) type CrashRestartFactory = Arc<dyn Fn(&[u8]) -> Option<TellIntent> + Send + Sync>;

pub(crate) type RouteFn<E> = Arc<dyn Fn(&E) -> Option<SagaId> + Send + Sync>;

/// Optional decode-failure routing function (ARCH-REVIEW-002).
///
/// When two sagas subscribe to the same upstream stream and a corrupt event
/// arrives, the transform delivers `UpstreamMessage::DecodeFailed` to every
/// subscribed saga because the typed event needed for `route_fn` cannot be
/// produced.  Providing this function lets a saga opt out of recording a DLQ
/// entry when the decode failure belongs to a different saga instance.
///
/// Return `Some(saga_id)` to accept the failure (records DLQ only when
/// `saga_id == self.saga_id`).  Return `None` to decline (no DLQ).
/// When absent, every `DecodeFailed` is recorded against this saga (legacy
/// behaviour).
pub(crate) type DecodeFailureRouteFn = Arc<dyn Fn(&AggregateId, u64) -> Option<SagaId> + Send + Sync>;

pub struct SagaProcess<S: Saga> {
    pub(crate) state: S,
    pub(crate) saga_id: SagaId,
    pub(crate) store: Arc<dyn EventStore>,
    pub(crate) codec: Arc<dyn ErasedCodec<S::Event>>,
    pub(crate) route_fn: RouteFn<S::SubscribedEvent>,
    pub(crate) decode_failure_route_fn: Option<DecodeFailureRouteFn>,
    pub(crate) sequence: u64,
    pub(crate) retry_policy: RetryPolicy,
    pub(crate) tell_states: HashMap<u64, TellState>,
    pub(crate) crash_restart_factory: Option<CrashRestartFactory>,
    pub(crate) lifecycle: Lifecycle,
    pub(crate) upstream_config: DurableSubscription<UpstreamMessage<S::SubscribedEvent>>,
    pub(crate) upstream_cursor: SequenceCursor,
    pub(crate) scheduler: Option<SchedulerProxy>,
    pub(crate) enqueue_policy: Arc<dyn EnqueuePolicy>,
    /// DLQ child poller spawner (ARCH-REVIEW-009).
    ///
    /// When `with_dead_letter_subscriber` is configured, `on_start` spawns the
    /// DLQ direct-poller as a child so the saga stop's cascade stops the poller
    /// automatically.  `None` when no subscriber was registered.
    pub(crate) dlq_child_spawn: Option<Arc<dyn DlqChildSpawn<Self>>>,
}

/// A timer firing delivered to the saga: the scheduled message payload keyed by
/// its [`TimerName`].  Constructed by the type-erased dispatch closure the
/// scheduler invokes when a timer elapses.
pub(crate) struct FireScheduled {
    pub(crate) name: TimerName,
    pub(crate) payload: Bytes,
}

/// Build the type-erased dispatch that the resident scheduler invokes when a
/// timer fires.  Captures a clone of the saga's own typed proxy so the
/// scheduler stays saga-type agnostic.
pub(crate) fn build_fire_dispatch<S: Saga>(
    proxy: ProcessProxy<SagaProcess<S>>,
    name: TimerName,
    payload: Bytes,
) -> DispatchFn {
    Arc::new(move || -> BoxFuture<'static, ()> {
        let proxy = proxy.clone();
        let name = name.clone();
        let payload = payload.clone();
        Box::pin(async move {
            if let Err(e) = proxy.tell(FireScheduled { name, payload }).await {
                tracing::warn!(
                    error = ?e,
                    "scheduler failed to deliver a timer firing to the saga; \
                     it may be stopping"
                );
            }
        })
    })
}

impl<S: Saga> SagaProcess<S> {
    fn timers(&self) -> Option<Timers> {
        self.scheduler
            .as_ref()
            .map(|s| Timers::new(self.saga_id.clone(), s.clone()))
    }

    /// Re-register schedules that replay found still active (persisted
    /// `Scheduled` with no matching `Cancelled` / `Fired`).  The remaining
    /// delay is recomputed from the persisted wall-clock anchor; a past
    /// deadline fires immediately.
    async fn reregister_active_schedules(
        &self,
        active: Vec<ActiveSchedule>,
        ctx: &mut ProcessContext<Self>,
    ) {
        let Some(timers) = self.timers() else {
            if !active.is_empty() {
                tracing::warn!(
                    saga_id = self.saga_id.as_str(),
                    count = active.len(),
                    "saga replay found active schedules but no scheduler was injected; \
                     they will not fire in this incarnation"
                );
            }
            return;
        };
        let now_millis = jiff::Timestamp::now().as_millisecond();
        for sched in active {
            // Saturating cast: if `after` exceeds i64::MAX milliseconds (~292 million
            // years) clamp to i64::MAX so saturating_add does not overflow.
            let after_ms: i64 = i64::try_from(sched.after.as_millis()).unwrap_or(i64::MAX);
            let deadline = sched
                .scheduled_at_unix_millis
                .saturating_add(after_ms);
            let remaining = deadline - now_millis;
            let after = if remaining <= 0 {
                Duration::ZERO
            } else {
                // remaining > 0 here; cast to u64 is safe.
                Duration::from_millis(remaining as u64)
            };
            let dispatch =
                build_fire_dispatch::<S>(ctx.self_proxy().clone(), sched.name.clone(), sched.payload);
            timers.start_single(sched.name, after, dispatch).await;
        }
    }

    fn drain_failed_tell_ids(&mut self) -> Vec<u64> {
        let failed: Vec<u64> = self
            .tell_states
            .iter()
            .filter(|(_, state)| matches!(state, TellState::Failed))
            .map(|(tell_id, _)| *tell_id)
            .collect();
        for tell_id in &failed {
            self.tell_states.remove(tell_id);
        }
        failed
    }

    fn ready_to_stop(&self) -> bool {
        matches!(self.lifecycle, Lifecycle::Draining) && in_flight_count(&self.tell_states) == 0
    }

    /// Route a saga failure to the DLQ (policy-gated append on the saga's own
    /// stream).  The single per-process entry point so every failure site funnels
    /// through one named operation.
    ///
    /// Returns `true` when the dead letter was written (or ignored by policy),
    /// and `false` when the policy required writing but the store append failed.
    /// Callers that receive `false` **must** stop the process so the upstream
    /// message is not treated as processed (G-27 persisted DLQ contract).
    async fn record_dead_letter(&mut self, failure: SagaFailure, source: SourceContext) -> bool {
        !matches!(
            enqueue_dead_letter(
                &self.store,
                &self.saga_id,
                &mut self.sequence,
                self.enqueue_policy.as_ref(),
                failure,
                source,
            )
            .await,
            EnqueueOutcome::AppendFailed
        )
    }
}

impl<S: Saga> Process for SagaProcess<S> {
    async fn on_start(&mut self, ctx: &mut ProcessContext<Self>) {
        let factory_ref = self.crash_restart_factory.as_ref().map(|f| f.as_ref());
        let outcome = match replay_and_redispatch(
            &self.saga_id,
            &mut self.state,
            self.codec.as_ref(),
            &self.store,
            &mut self.sequence,
            &mut self.tell_states,
            factory_ref,
            self.retry_policy.clone(),
            self.enqueue_policy.as_ref(),
            ctx,
        )
        .await
        {
            Ok(o) => o,
            // store.load() failed — the terminated state cannot be determined.
            // Fail safe: stop the process without subscribing so a possibly-
            // terminated saga is not revived (D-14).
            Err(()) => {
                tracing::error!(
                    saga_id = self.saga_id.as_str(),
                    "saga replay failed; stopping to prevent spawn with unknown terminated state"
                );
                if let Err(e) = ctx.stop_self().await {
                    tracing::warn!(error = %e, "saga replay-failed stop_self failed");
                }
                return;
            }
        };
        for tell_id in outcome.failed {
            self.tell_states.insert(tell_id, TellState::Failed);
        }

        // D-14: a saga whose stream carries a durable `Ended` marker terminated
        // in a previous incarnation.  Refuse to wire up the upstream
        // subscription — so `Saga::handle` can never run again — and stop the
        // inert process to release its resources.
        if outcome.ended {
            tracing::info!(
                saga_id = self.saga_id.as_str(),
                "saga already terminated (Ended marker present); skipping subscription"
            );
            if let Err(e) = ctx.stop_self().await {
                tracing::warn!(error = %e, "terminated saga stop_self failed");
            }
            return;
        }

        // Re-register timers that were still pending at the previous
        // incarnation's stop (E-26): unfired, uncancelled schedules found on the
        // saga's own stream.
        self.reregister_active_schedules(outcome.active_schedules, ctx)
            .await;

        self.upstream_config
            .spawn_child(ctx, self.upstream_cursor.clone())
            .await;

        // G-29 / ARCH-REVIEW-009: spawn the DLQ direct-poller as a child of
        // this saga process so that when the saga stops the runtime
        // cascade-stops the DLQ poller automatically.  Without child binding
        // the poller would outlive the saga as long as the subscriber is alive.
        if let Some(ref dlq) = self.dlq_child_spawn {
            dlq.spawn_as_child(ctx, &self.saga_id, &self.store).await;
        }
    }

    async fn on_stop(&mut self, _ctx: &mut ProcessContext<Self>) {
        // Saga lifecycle teardown (E-25): cancel every timer this saga owns so
        // the resident scheduler does not fire into a stopped process.  This is
        // an in-memory scheduler cancel only — no `Cancelled` marker is
        // persisted, so a future incarnation replays and re-registers any still
        // unfired schedule.
        if let Some(timers) = self.timers() {
            timers.cancel_all().await;
        }
    }
}

impl<S: Saga> Receive<UpstreamMessage<S::SubscribedEvent>> for SagaProcess<S> {
    type Response = ();
    /// Returning `Err(SagaUpstreamHandlerError)` signals the `DirectPoller`
    /// (via `ask()`) to NOT advance the upstream cursor (G-27 state-consistency).
    type Error = SagaUpstreamHandlerError;

    async fn recv(
        &mut self,
        msg: UpstreamMessage<S::SubscribedEvent>,
        ctx: &mut ProcessContext<Self>,
    ) -> Result<(), SagaUpstreamHandlerError> {
        // G-26 / G-30 immediate: the upstream codec could not decode the event.
        // Without a typed event routing is impossible, so the failure is
        // delivered to this saga for DLQ recording unless a
        // `decode_failure_route_fn` is configured that routes it elsewhere
        // (ARCH-REVIEW-002: prevent every subscribed saga from recording a
        // duplicate DLQ entry for the same corrupt upstream event).
        let (aggregate_id, sequence, event, raw_payload) = match msg {
            UpstreamMessage::DecodeFailed {
                aggregate_id,
                sequence,
                error,
            } => {
                // When a decode-failure route is configured, honour it.
                // `None` → this saga opts out; `Some(id)` where id ≠ saga_id
                // → another saga owns this failure, skip silently.
                if let Some(ref route) = self.decode_failure_route_fn {
                    match route(&aggregate_id, sequence) {
                        Some(ref target_id) if target_id != &self.saga_id => {
                            return Ok(());
                        }
                        None => {
                            return Ok(());
                        }
                        _ => {}
                    }
                }
                let source = SourceContext {
                    aggregate_id,
                    upstream_sequence: sequence,
                };
                if !self
                    .record_dead_letter(SagaFailure::DecodeFailed { error }, source)
                    .await
                {
                    // G-27: return Err so the DirectPoller (via ask()) does NOT
                    // advance the upstream cursor — the runtime will stop this
                    // process via handler-failure supervision.
                    tracing::error!(
                        saga_id = self.saga_id.as_str(),
                        "saga DLQ append failed for DecodeFailed; signalling upstream handler error (G-27)"
                    );
                    return Err(SagaUpstreamHandlerError);
                }
                return Ok(());
            }
            UpstreamMessage::Decoded {
                aggregate_id,
                sequence,
                event,
                raw_payload,
            } => (aggregate_id, sequence, event, raw_payload),
        };

        let Some(target_id) = (self.route_fn)(&event) else {
            return Ok(());
        };
        if target_id != self.saga_id {
            return Ok(());
        }

        let source = SourceContext {
            aggregate_id: aggregate_id.clone(),
            upstream_sequence: sequence,
        };

        // D-14 / ARCH-001: once `End` has been interpreted and the durable
        // `Ended` marker persisted, the lifecycle transitions to `Draining`
        // while in-flight tells settle.  Upstream events routed here during
        // draining must not reach `Saga::handle` — doing so could produce
        // additional effects that violate the terminal lifecycle contract.  The
        // dropped message is recorded as a dead letter (G-30 immediate) with
        // its raw wire payload preserved (G-28).
        if matches!(self.lifecycle, Lifecycle::Draining) {
            if !self
                .record_dead_letter(
                    SagaFailure::EndedSagaReceivedMessage { message: raw_payload },
                    source,
                )
                .await
            {
                // G-27: return Err so the DirectPoller (via ask()) does NOT
                // advance the upstream cursor — the runtime will stop this
                // process via handler-failure supervision.
                tracing::error!(
                    saga_id = self.saga_id.as_str(),
                    "saga DLQ append failed for EndedSagaReceivedMessage; signalling upstream handler error (G-27)"
                );
                return Err(SagaUpstreamHandlerError);
            }
            return Ok(());
        }

        let current_sequence = self.sequence;
        let drained_failed = self.drain_failed_tell_ids();
        let mut saga_ctx = SagaContext::new(
            self.saga_id.clone(),
            current_sequence,
            aggregate_id.clone(),
            sequence,
            jiff::Timestamp::now(),
            drained_failed,
        );
        let effect = match self.state.handle(event, &mut saga_ctx).await {
            Ok(effect) => effect,
            Err(e) => {
                tracing::warn!(error = %e, "saga handle failed");
                if !self
                    .record_dead_letter(SagaFailure::HandleFailed { error: e.to_string() }, source)
                    .await
                {
                    // G-27: return Err so the DirectPoller (via ask()) does NOT
                    // advance the upstream cursor — the runtime will stop this
                    // process via handler-failure supervision.
                    tracing::error!(
                        saga_id = self.saga_id.as_str(),
                        "saga DLQ append failed for HandleFailed; signalling upstream handler error (G-27)"
                    );
                    return Err(SagaUpstreamHandlerError);
                }
                return Ok(());
            }
        };

        let mut ictx = InterpreterCtx {
            state: &mut self.state,
            saga_id: self.saga_id.clone(),
            sequence: &mut self.sequence,
            store: Arc::clone(&self.store),
            codec: Arc::clone(&self.codec),
            retry_policy: self.retry_policy.clone(),
            process_ctx: ctx,
            tell_states: &mut self.tell_states,
            lifecycle: &mut self.lifecycle,
            scheduler: self.scheduler.clone(),
            enqueue_policy: Arc::clone(&self.enqueue_policy),
        };
        // G-27: if the interpreter signals DlqFailed (PersistFailed DLQ
        // append also failed), propagate Err so the DirectPoller (via ask())
        // does NOT advance the upstream cursor.
        if matches!(
            run_saga_effect(effect, &mut ictx).await,
            InterpretOutcome::DlqFailed
        ) {
            tracing::error!(
                saga_id = self.saga_id.as_str(),
                "saga DLQ append failed for PersistFailed (upstream handler); \
                 signalling upstream handler error (G-27)"
            );
            return Err(SagaUpstreamHandlerError);
        }
        Ok(())
    }
}

impl<S: Saga> Receive<FireScheduled> for SagaProcess<S> {
    type Response = ();
    type Error = std::convert::Infallible;

    async fn recv(
        &mut self,
        msg: FireScheduled,
        ctx: &mut ProcessContext<Self>,
    ) -> Result<(), std::convert::Infallible> {
        // ARCH-001: mirror the `EventEnvelope` guard — timer firings must also
        // be dropped while draining so `on_scheduled` cannot produce additional
        // effects after the terminal `Ended` marker has been written.  The
        // dropped firing is recorded as a dead letter (its raw payload retained).
        if matches!(self.lifecycle, Lifecycle::Draining) {
            if !self
                .record_dead_letter(
                    SagaFailure::EndedSagaReceivedMessage {
                        message: msg.payload.clone(),
                    },
                    SourceContext::without_upstream(),
                )
                .await
            {
                // G-27: DLQ append failed — stop so the timer firing is not
                // treated as processed; a supervised restart will retry.
                tracing::error!(
                    saga_id = self.saga_id.as_str(),
                    "saga DLQ append failed for EndedSagaReceivedMessage (timer); stopping for supervised restart"
                );
                let _ = ctx.stop_self().await;
            }
            return Ok(());
        }

        let FireScheduled { name, payload } = msg;
        let message: S::ScheduledMessage = match serde_json::from_slice(&payload) {
            Ok(message) => message,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "saga scheduled payload failed to deserialize; dropping timer firing"
                );
                if !self
                    .record_dead_letter(
                        SagaFailure::DecodeFailed { error: e.to_string() },
                        SourceContext::without_upstream(),
                    )
                    .await
                {
                    // G-27: DLQ append failed — stop so the timer firing is not
                    // treated as processed; a supervised restart will retry.
                    tracing::error!(
                        saga_id = self.saga_id.as_str(),
                        "saga DLQ append failed for DecodeFailed (timer); stopping for supervised restart"
                    );
                    let _ = ctx.stop_self().await;
                }
                return Ok(());
            }
        };

        let current_sequence = self.sequence;
        let drained_failed = self.drain_failed_tell_ids();
        let mut saga_ctx = SagaContext::new(
            self.saga_id.clone(),
            current_sequence,
            AggregateId::new(""),
            0,
            jiff::Timestamp::now(),
            drained_failed,
        );
        let effect = match self.state.on_scheduled(message, &mut saga_ctx).await {
            Ok(effect) => effect,
            Err(e) => {
                tracing::warn!(error = %e, "saga on_scheduled failed");
                if !self
                    .record_dead_letter(
                        SagaFailure::ScheduledFailed { error: e.to_string() },
                        SourceContext::without_upstream(),
                    )
                    .await
                {
                    // G-27: DLQ append failed — stop so the timer firing is not
                    // treated as processed; a supervised restart will retry.
                    tracing::error!(
                        saga_id = self.saga_id.as_str(),
                        "saga DLQ append failed for ScheduledFailed; stopping for supervised restart"
                    );
                    let _ = ctx.stop_self().await;
                }
                return Ok(());
            }
        };

        // Record the fire durably BEFORE running the handler's effect so that
        // if `on_scheduled` re-schedules under the same `TimerName` the event
        // stream order becomes:
        //
        //   … Scheduled → Fired → (new) Scheduled
        //
        // Replay folds these left-to-right per name:
        //   insert(name) → remove(name) → insert(name)  ⟹  net: active (correct)
        //
        // The opposite order (… Scheduled → (new) Scheduled → Fired) would
        // cause replay to remove the new schedule when it processes the Fired
        // marker, breaking E-28 / E-26.
        //
        // Best-effort once: an append failure is tolerated (at-least-once
        // delivery permits a replay re-fire).
        let fired_event = ScheduleEvent::Fired {
            token: ScheduleToken {
                saga_id: self.saga_id.clone(),
                name,
            },
            fired_at_unix_millis: jiff::Timestamp::now().as_millisecond(),
        };
        append_schedule_marker(
            &self.store,
            &self.saga_id,
            &mut self.sequence,
            fired_event,
            jiff::Timestamp::now(),
        )
        .await;

        let mut ictx = InterpreterCtx {
            state: &mut self.state,
            saga_id: self.saga_id.clone(),
            sequence: &mut self.sequence,
            store: Arc::clone(&self.store),
            codec: Arc::clone(&self.codec),
            retry_policy: self.retry_policy.clone(),
            process_ctx: ctx,
            tell_states: &mut self.tell_states,
            lifecycle: &mut self.lifecycle,
            scheduler: self.scheduler.clone(),
            enqueue_policy: Arc::clone(&self.enqueue_policy),
        };
        // FireScheduled is not delivered via DirectPoller so there is no
        // upstream cursor to withhold; call stop_self() to ensure the process
        // does not silently continue when a PersistFailed DLQ append also fails.
        if matches!(
            run_saga_effect(effect, &mut ictx).await,
            InterpretOutcome::DlqFailed
        ) {
            tracing::error!(
                saga_id = self.saga_id.as_str(),
                "saga DLQ append failed for PersistFailed (timer handler); stopping for supervised restart"
            );
            let _ = ctx.stop_self().await;
        }
        Ok(())
    }
}

pub(crate) struct OutboxReport {
    pub(crate) tell_id: u64,
    pub(crate) outcome: TellOutcome,
}

async fn append_terminal_and_claim(
    store: &Arc<dyn EventStore>,
    saga_id: &SagaId,
    sequence: &mut u64,
    outcome: TellOutcome,
    tell_id: u64,
) -> bool {
    let candidate = *sequence + 1;
    let appended =
        OutboxAppender::append_terminal(store, saga_id, candidate, outcome, tell_id).await;
    if appended {
        *sequence = candidate;
    }
    appended
}

impl<S: Saga> Receive<OutboxReport> for SagaProcess<S> {
    type Response = ();
    type Error = std::convert::Infallible;

    async fn recv(
        &mut self,
        msg: OutboxReport,
        ctx: &mut ProcessContext<Self>,
    ) -> Result<(), std::convert::Infallible> {
        let appended = append_terminal_and_claim(
            &self.store,
            &self.saga_id,
            &mut self.sequence,
            msg.outcome,
            msg.tell_id,
        )
        .await;

        if appended {
            match msg.outcome {
                TellOutcome::Acked => {
                    self.tell_states.remove(&msg.tell_id);
                }
                TellOutcome::Failed => {
                    // Capture target_id and crash-restart payload from the
                    // in-memory intent before the state entry is replaced.
                    let intent_data = match self.tell_states.get(&msg.tell_id) {
                        Some(TellState::Pending(intent)) | Some(TellState::AppendFailed(intent)) => {
                            let target = intent.target_id.clone();
                            let message = intent.crash_restart_payload.clone().unwrap_or_default();
                            Some((target, message))
                        }
                        _ => {
                            tracing::warn!(
                                tell_id = msg.tell_id,
                                "saga: TellFailed received for unknown or already-failed tell; \
                                 DLQ entry skipped (target not available)"
                            );
                            None
                        }
                    };
                    self.tell_states.insert(msg.tell_id, TellState::Failed);
                    if let Some((target, message)) = intent_data {
                        // The terminal `tell_failed` outbox marker is already
                        // persisted above (staged retry exhausted), so this dead
                        // letter lands *after* it — G-30 timing.
                        if !self
                            .record_dead_letter(
                                SagaFailure::TellFailed { target, message },
                                SourceContext::without_upstream(),
                            )
                            .await
                        {
                            // G-27: DLQ append failed — stop so the outcome is
                            // not treated as processed; a supervised restart
                            // will retry from the terminal outbox marker.
                            tracing::error!(
                                saga_id = self.saga_id.as_str(),
                                tell_id = msg.tell_id,
                                "saga DLQ append failed for TellFailed; stopping for supervised restart"
                            );
                            let _ = ctx.stop_self().await;
                        }
                    }
                }
            }
        } else {
            match self.tell_states.remove(&msg.tell_id) {
                Some(TellState::Pending(intent)) => {
                    self.tell_states
                        .insert(msg.tell_id, TellState::AppendFailed(intent));
                }
                Some(other) => {
                    self.tell_states.insert(msg.tell_id, other);
                }
                None => {}
            }
            tracing::debug!(
                tell_id = msg.tell_id,
                "saga outbox terminal append failed; \
                 tell state preserved as AppendFailed for supervised-restart re-dispatch"
            );
        }

        if self.ready_to_stop() {
            if let Err(e) = ctx.stop_self().await {
                tracing::warn!(
                    error = %e,
                    "saga deferred End: stop_self failed after all outbox executors settled"
                );
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::pin::Pin;
    use std::sync::atomic::{AtomicBool, Ordering};

    use async_trait::async_trait;
    use futures_core::Stream;
    use nitinol_persistence::error::{AppendError, LoadError};
    use nitinol_persistence::store::EventStream;
    use nitinol_persistence::{AppendOutcome, AppendingEvent, LoadQuery};

    struct FailOnceStore {
        has_failed: AtomicBool,
    }

    impl FailOnceStore {
        fn new() -> Self {
            Self {
                has_failed: AtomicBool::new(false),
            }
        }
    }

    #[async_trait]
    impl EventStore for FailOnceStore {
        async fn append(
            &self,
            _key: &str,
            _events: Vec<AppendingEvent>,
        ) -> Result<AppendOutcome, AppendError> {
            if !self.has_failed.swap(true, Ordering::SeqCst) {
                Err(AppendError::Backend("injected failure".into()))
            } else {
                Ok(AppendOutcome {
                    assigned_sequences: vec![],
                    stream_version: 0,
                })
            }
        }

        async fn load(&self, _query: LoadQuery) -> Result<EventStream<'_>, LoadError> {
            let stream: Pin<
                Box<
                    dyn Stream<Item = Result<nitinol_persistence::LoadedEvent, LoadError>>
                        + Send
                        + '_,
                >,
            > = Box::pin(futures_util::stream::empty());
            Ok(stream)
        }
    }

    #[tokio::test]
    async fn append_terminal_does_not_advance_sequence_on_store_failure() {
        let store: Arc<dyn EventStore> = Arc::new(FailOnceStore::new());
        let saga_id = SagaId::new("sequence-skip-regression");
        let mut sequence: u64 = 0;

        let ok = append_terminal_and_claim(&store, &saga_id, &mut sequence, TellOutcome::Acked, 1)
            .await;
        assert!(!ok, "must return false when EventStore::append fails");
        assert_eq!(sequence, 0, "sequence must not advance on store failure");

        let ok = append_terminal_and_claim(&store, &saga_id, &mut sequence, TellOutcome::Acked, 1)
            .await;
        assert!(ok, "must return true when EventStore::append succeeds");
        assert_eq!(
            sequence, 1,
            "sequence must be 1 (no gap from the failed attempt)"
        );
    }
}
