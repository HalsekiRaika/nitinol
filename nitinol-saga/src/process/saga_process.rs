use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use nitinol_eventsource::{DurableSubscription, SequenceCursor, SystemEvent};
use nitinol_persistence::error::AppendError;
use nitinol_persistence::store::EventStore;
use nitinol_persistence::{AggregateId, LoadedEvent};
use nitinol_runtime::process::{Process, ProcessContext, Receive};

use crate::components::{deadline, PendingDeadlineCell};
use crate::consumed::{ConsumedPositionEvent, ConsumedPositions, PositionSource};
use crate::context::SagaContext;
use crate::dead_letter::{
    self, DeadLetterDispositionEvent, DlqChildSpawn, EnqueueOutcome, EnqueuePolicy,
    RecordDisposition, SagaFailure, SourceContext,
};
use crate::effect::TellIntent;
use crate::error::SagaUpstreamHandlerError;
use crate::id::SagaId;
use crate::journal::ActiveSchedule;
use crate::observability::report_system_event;
use crate::outbox::{OutboxAppender, RetryPolicy, TellOutcome};
use crate::process::instance::ScheduleDelivery;
use crate::process::interpreter::{self, InterpretOutcome, InterpreterCtx};
use crate::process::replay;
use crate::saga::Saga;
use crate::scheduler::{self, ScheduleEvent, SchedulerProxy, Timers};
use crate::scheduler::{ScheduleToken, TimerName};
use crate::subscribe::{DeclaresDeadline, SagaSubscribe, SubscribedSource};

/// One observed record, decoded as the variant of the saga's alphabet it
/// belongs to, delivered by the `DirectPollerProcess` of the source that read
/// it.
///
/// A record whose bytes could not be decoded never becomes one of these: the
/// transform reports it and answers `None`, which advances the poller's cursor
/// past it.  A decode failure is deterministic and permanent, and carries no
/// typed input to correlate, so there is no instance it could be attributed to
/// and nothing a retry would change.
pub(crate) struct UpstreamMessage<A> {
    pub(crate) aggregate_id: AggregateId,
    pub(crate) sequence: u64,
    /// Which declared source read this record.
    ///
    /// Decided by the transform — the source that matched the row's event type —
    /// so no later stage has to classify it again.  It travels with the record
    /// because the consumed position names it: two declared sources can share
    /// one stream, and the stream key alone would let one source's progress
    /// cover the other's records.
    pub(crate) source: PositionSource,
    pub(crate) input: A,
    /// Raw bytes as loaded from the upstream EventStore, preserved through
    /// the transform so the dead letter carries the actual payload.
    pub(crate) raw_payload: Bytes,
}

/// The subscription one declared source is observed through.
///
/// One of these per declared external source: its own transform, its own
/// cursor.  A saga's sources are independent, so a source whose upstream has
/// stalled cannot hold up another's progress.
pub(crate) struct OwnedUpstream<A> {
    pub(crate) config: DurableSubscription<UpstreamMessage<A>>,
    pub(crate) cursor: SequenceCursor,
    /// The declared source this subscription reads, so the cursor can be
    /// resumed past what *this* source has consumed and no other.
    pub(crate) source: PositionSource,
}

impl<A> Clone for OwnedUpstream<A> {
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            cursor: self.cursor.clone(),
            source: self.source.clone(),
        }
    }
}

/// Build the subscription that turns each raw record of one declared source into
/// the [`UpstreamMessage`] a saga understands.
///
/// Owned here rather than at either spawn boundary so a standalone saga and a
/// manager-fronted one classify the same record — wrong type, decodable,
/// corrupt — identically.
pub(crate) fn source_subscription<A>(source: SubscribedSource<A>) -> OwnedUpstream<A>
where
    A: Send + Sync + 'static,
{
    let event_type = source.event_type();
    let position_source = PositionSource::of(event_type);
    let (decode, store, cursor) = source.into_parts();
    let of_source = position_source.clone();
    OwnedUpstream {
        config: DurableSubscription::new(store, move |loaded: LoadedEvent| {
            // This source claims the rows of its own event type and no others,
            // which is what lets several sources read one stream key without
            // reading each other's records.
            if loaded.event_type != event_type {
                return None;
            }
            // Capture the raw payload before decoding so the draining branch can
            // preserve it in `SagaFailure::EndedSagaReceivedMessage`.
            let raw_payload = loaded.payload.clone();
            let aggregate_id = AggregateId::new(loaded.stream_key);
            let sequence = loaded.sequence;
            match decode(&loaded.payload) {
                Ok(input) => Some(UpstreamMessage {
                    aggregate_id,
                    sequence,
                    source: of_source.clone(),
                    input,
                    raw_payload,
                }),
                Err(e) => {
                    // Reported and skipped, which is the whole of the handling:
                    // answering `None` advances this source's cursor past the
                    // record. A codec that refused these bytes once refuses them
                    // every time, and with no typed input there is no instance
                    // to attribute the failure to — so there is nothing a
                    // retry, a dead letter or a held cursor could recover.
                    // Nothing durable is written, so a restart re-reads the
                    // record and repeats this report.
                    tracing::error!(
                        error = %e,
                        event_type = %event_type,
                        upstream_key = aggregate_id.as_str(),
                        upstream_sequence = sequence,
                        "saga could not decode an observed record; skipping it",
                    );
                    None
                }
            }
        }),
        cursor,
        source: position_source,
    }
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

impl TellState {
    /// The intent this tell can still be re-dispatched from, if any.
    ///
    /// A tell that is pending and one whose terminal marker failed to append
    /// both still hold what they were told to send; a failed one has given it
    /// up, so there is nothing to re-dispatch.
    pub(crate) fn intent(&self) -> Option<&TellIntent> {
        match self {
            TellState::Pending(intent) | TellState::AppendFailed(intent) => Some(intent),
            TellState::Failed => None,
        }
    }

    /// Is this tell still out, with its outcome unknown?
    ///
    /// Only `Pending` is: `AppendFailed` and `Failed` have both reached their
    /// outcome, and what is left of them is a marker to write rather than a
    /// delivery to wait for.
    pub(crate) fn is_in_flight(&self) -> bool {
        matches!(self, TellState::Pending(_))
    }
}

pub(crate) fn in_flight_count(tell_states: &HashMap<u64, TellState>) -> usize {
    tell_states
        .values()
        .filter(|state| state.is_in_flight())
        .count()
}

pub(crate) type CrashRestartFactory = Arc<dyn Fn(&[u8]) -> Option<TellIntent> + Send + Sync>;

pub struct SagaProcess<S: Saga> {
    pub(crate) state: S,
    pub(crate) saga_id: SagaId,
    pub(crate) store: Arc<dyn EventStore>,
    pub(crate) sequence: u64,
    /// How far each upstream source this saga reads has been consumed.
    ///
    /// Restored by replay from the saga's own stream and advanced by every
    /// batch that lands, so a record already consumed — whether by a
    /// [`Saga::handle`] decision or by a dead-letter append that settled it
    /// without one — is dropped before it reaches [`Saga::handle`] again,
    /// whether the position was folded out of the journal or written by this
    /// incarnation.
    pub(crate) consumed: ConsumedPositions,
    pub(crate) retry_policy: RetryPolicy,
    pub(crate) tell_states: HashMap<u64, TellState>,
    pub(crate) crash_restart_factory: Option<CrashRestartFactory>,
    pub(crate) lifecycle: Lifecycle,
    /// One subscription per declared source, each cascade-stopped with this
    /// process.
    ///
    /// Empty on an instance a `SagaManagerProps` manager spawned: the manager
    /// holds the subscriptions, so an instance that opened its own would decode
    /// every observed record a second time.  [`Self::owns_its_subscriptions`] is
    /// how that is read.
    pub(crate) upstream: Vec<OwnedUpstream<S::Subscribe>>,
    /// The resident timer core, present exactly when this saga's alphabet
    /// declares a deadline — resolved at the spawn boundary, never handed in by
    /// the user.
    pub(crate) scheduler: Option<SchedulerProxy>,
    /// What the journal says is still pending, shared with the saga's own
    /// [`Deadline`](crate::Deadline) component so it reads the stream rather
    /// than its own intent.
    pub(crate) pending_deadlines: PendingDeadlineCell,
    /// Where a deadline's firing is addressed, and so whether this instance's
    /// deadlines survive it stopping.
    pub(crate) delivery: ScheduleDelivery<S>,
    pub(crate) enqueue_policy: Arc<dyn EnqueuePolicy>,
    /// DLQ child poller spawner.
    ///
    /// When `with_dead_letter_subscriber` is configured, `on_start` spawns the
    /// DLQ direct-poller as a child so the saga stop's cascade stops the poller
    /// automatically.  `None` when no subscriber was registered.
    pub(crate) dlq_child_spawn: Option<Arc<dyn DlqChildSpawn<Self>>>,
}

/// A timer firing delivered to the saga: the scheduled message payload keyed by
/// its [`TimerName`].  Constructed by the type-erased dispatch closure the
/// scheduler invokes when a deadline is reached.
///
/// `Clone` so a manager-fronted delivery that finds its cached instance proxy
/// already stale can retry the same firing against the instance it respawns,
/// without losing this delivery attempt — see
/// `SagaManagerProcess::recv(DeliverScheduled)`.
#[derive(Clone)]
pub(crate) struct FireScheduled {
    pub(crate) name: TimerName,
    pub(crate) payload: Bytes,
}

impl<S: Saga> SagaProcess<S> {
    fn timers(&self) -> Option<Timers> {
        self.scheduler
            .as_ref()
            .map(|s| Timers::new(self.saga_id.clone(), s.clone()))
    }

    /// Does this instance hold its own subscriptions, or does a manager observe
    /// for it?
    ///
    /// The two differ in what a terminated instance may do with itself: one that
    /// owns its subscriptions can decline to wire them and stop, because then
    /// nothing can reach it; one a manager fronts keeps receiving whatever the
    /// manager routes and has to stay resident to record it.
    fn owns_its_subscriptions(&self) -> bool {
        !self.upstream.is_empty()
    }

    /// Re-register schedules that replay found still active (persisted
    /// `Scheduled` with no matching `Cancelled` / `Fired`), and make them the
    /// pending view this incarnation answers from.
    ///
    /// The remaining delay is recomputed from the persisted wall-clock anchor; a
    /// past deadline fires immediately.
    async fn reregister_active_schedules(
        &self,
        active: Vec<ActiveSchedule>,
        ctx: &mut ProcessContext<Self>,
    ) {
        // A fresh incarnation knows nothing of what it set, so the journal is
        // the whole of the answer — which is why this replaces the view rather
        // than adding to it.
        deadline::restore_pending(
            &self.pending_deadlines,
            active.iter().map(|sched| {
                (
                    sched.name.clone(),
                    deadline::due_at(sched.scheduled_at_unix_millis, sched.after),
                )
            }),
        );

        let Some(timers) = self.timers() else {
            if !active.is_empty() {
                // Reachable only where the stream disagrees with the alphabet:
                // schedule facts written by an incarnation whose alphabet
                // declared a deadline, replayed by one whose does not. The host
                // resolves the timer core from the declaration, so a saga that
                // still declares one always has it.
                tracing::warn!(
                    saga_id = self.saga_id.as_str(),
                    count = active.len(),
                    "saga replay found active schedules but its alphabet declares no \
                     deadline; they will not fire"
                );
            }
            return;
        };
        let now_millis = jiff::Timestamp::now().as_millisecond();
        for sched in active {
            // Saturating cast: if `after` exceeds i64::MAX milliseconds (~292 million
            // years) clamp to i64::MAX so saturating_add does not overflow.
            let after_ms: i64 = i64::try_from(sched.after.as_millis()).unwrap_or(i64::MAX);
            let deadline = sched.scheduled_at_unix_millis.saturating_add(after_ms);
            let remaining = deadline - now_millis;
            let after = if remaining <= 0 {
                Duration::ZERO
            } else {
                // remaining > 0 here; cast to u64 is safe.
                Duration::from_millis(remaining as u64)
            };
            let dispatch =
                self.delivery
                    .dispatch(&self.saga_id, ctx, sched.name.clone(), sched.payload);
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
    /// stream) — plus the consumed position, when `input_position` names the
    /// upstream record this failure settles.  The single per-process entry
    /// point so every failure site funnels through one named operation.
    ///
    /// `input_position` is `Some` exactly where this call is the `Ok` outcome
    /// a `DirectPoller` will advance its cursor past: the record must become
    /// durably consumed in the same call, or a restart before the next
    /// incarnation's own progress covers it would redeliver it into the same
    /// failure forever.  Every other caller — tell-outcome reports, timer
    /// firings, replay re-dispatch — has no upstream record to settle and
    /// passes `None`.
    ///
    /// Returns `true` when the dead letter (and position, if any) was written
    /// — or ignored by policy, which still durably records a bare position
    /// when one was given — and `false` when the store append failed.
    /// Callers that receive `false` **must** stop the process so the upstream
    /// message is not treated as processed — the persisted DLQ must stay
    /// authoritative.
    async fn record_dead_letter(
        &mut self,
        failure: SagaFailure,
        source: SourceContext,
        input_position: Option<ConsumedPositionEvent>,
    ) -> bool {
        !matches!(
            dead_letter::enqueue_dead_letter(
                &self.store,
                &self.saga_id,
                &mut self.sequence,
                self.enqueue_policy.as_ref(),
                failure,
                source,
                input_position.as_ref(),
                &mut self.consumed,
            )
            .await,
            EnqueueOutcome::AppendFailed
        )
    }

    /// Lend the interpreter everything it mutates on this process, so every
    /// entry point that runs a [`SagaEffect`] does so against the same state.
    ///
    /// `input_position` is the upstream record the effect was decided from —
    /// `None` at the entry points that have no upstream input, which is why
    /// what they persist records no position.
    fn interpreter_ctx<'a>(
        &'a mut self,
        process_ctx: &'a mut ProcessContext<Self>,
        input_position: Option<ConsumedPositionEvent>,
    ) -> InterpreterCtx<'a, S> {
        InterpreterCtx {
            state: &mut self.state,
            saga_id: self.saga_id.clone(),
            sequence: &mut self.sequence,
            store: Arc::clone(&self.store),
            retry_policy: self.retry_policy.clone(),
            process_ctx,
            tell_states: &mut self.tell_states,
            lifecycle: &mut self.lifecycle,
            scheduler: self.scheduler.clone(),
            pending_deadlines: Arc::clone(&self.pending_deadlines),
            delivery: &self.delivery,
            enqueue_policy: Arc::clone(&self.enqueue_policy),
            consumed: &mut self.consumed,
            input_position,
        }
    }

    /// Account for a tell whose terminal `tell_failed` marker has just been
    /// persisted: record it durably, then either notify the saga or — once the
    /// saga has ended — drop the report.
    ///
    /// The failure leaves this method through exactly one of two channels, so
    /// it is never announced twice: the entry is removed from `tell_states`
    /// when the hook is notified, and kept there for
    /// [`SagaContext::failed_tell_ids`] otherwise.
    async fn settle_failed_tell(&mut self, tell_id: u64, ctx: &mut ProcessContext<Self>) {
        // The target and crash-restart payload live only in the in-memory
        // intent, so they are captured before the entry is disposed of.
        let captured = self
            .tell_states
            .get(&tell_id)
            .and_then(|state| match state {
                TellState::Pending(intent) | TellState::AppendFailed(intent) => Some((
                    intent.target_id.clone(),
                    intent.crash_restart_payload.clone().unwrap_or_default(),
                )),
                TellState::Failed => None,
            });
        let Some((target, message)) = captured else {
            tracing::warn!(
                tell_id,
                "saga: TellFailed received for unknown or already-failed tell; \
                 DLQ entry skipped (target not available)"
            );
            self.tell_states.insert(tell_id, TellState::Failed);
            return;
        };

        // The terminal `tell_failed` outbox marker is already persisted (staged
        // retry exhausted), so this dead letter lands after it — and before any
        // compensation the saga runs in response.
        if !self
            .record_dead_letter(
                SagaFailure::TellFailed {
                    target: target.clone(),
                    message,
                },
                SourceContext::without_upstream(),
                None,
            )
            .await
        {
            // DLQ append failed — the failure is not durably recorded, so it
            // must not drive a compensation.  Stop instead; a supervised
            // restart replays it from the terminal outbox marker.
            self.tell_states.insert(tell_id, TellState::Failed);
            tracing::error!(
                saga_id = self.saga_id.as_str(),
                tell_id,
                "saga DLQ append failed for TellFailed; stopping for supervised restart"
            );
            let _ = ctx.stop_self().await;
            return;
        }

        if matches!(self.lifecycle, Lifecycle::Draining) {
            // A saga that has already written its terminal marker must produce
            // no further effects, so the report is dropped; the dead letter
            // above is what keeps the failure observable.
            self.tell_states.insert(tell_id, TellState::Failed);
            tracing::debug!(
                tell_id,
                "saga ended while this tell was in flight; its failure is dropped \
                 instead of reaching the tell-failure hook"
            );
            return;
        }

        // Dropping the entry before the hook runs both keeps the failure out of
        // the drain path and lets an `End` returned by the hook stop the saga
        // at once rather than leaving it waiting on this settled tell.
        self.tell_states.remove(&tell_id);
        self.notify_tell_failed(target, tell_id, ctx).await;
    }

    /// Hand a settled tell failure to the saga and interpret whatever
    /// compensation it returns, so a saga whose upstream has gone quiet still
    /// reacts to the failure.
    ///
    /// `tell_id` reaches the saga through the context rather than the `target`
    /// argument because several tells may share one target aggregate.
    async fn notify_tell_failed(
        &mut self,
        target: AggregateId,
        tell_id: u64,
        ctx: &mut ProcessContext<Self>,
    ) {
        let current_sequence = self.sequence;
        let mut saga_ctx = SagaContext::new(
            self.saga_id.clone(),
            current_sequence,
            AggregateId::new(""),
            0,
            jiff::Timestamp::now(),
            vec![tell_id],
        );
        let effect = match self.state.on_tell_failed(target, &mut saga_ctx).await {
            Ok(effect) => effect,
            Err(e) => {
                tracing::warn!(error = %e, tell_id, "saga on_tell_failed failed");
                if !self
                    .record_dead_letter(
                        SagaFailure::TellFailedHookFailed {
                            error: e.to_string(),
                        },
                        SourceContext::without_upstream(),
                        None,
                    )
                    .await
                {
                    // DLQ append failed — stop so the rejected compensation is
                    // not treated as accounted for; a supervised restart
                    // replays the failure from the terminal outbox marker.
                    tracing::error!(
                        saga_id = self.saga_id.as_str(),
                        tell_id,
                        "saga DLQ append failed for TellFailedHookFailed; \
                         stopping for supervised restart"
                    );
                    let _ = ctx.stop_self().await;
                }
                return;
            }
        };

        // A tell failure is not an upstream record, so a compensation persisted
        // for it consumes nothing.
        let mut ictx = self.interpreter_ctx(ctx, None);
        // The outcome report does not arrive through the DirectPoller, so there
        // is no upstream cursor to withhold; stop instead when the compensation
        // could neither be persisted nor dead-lettered.
        if matches!(
            interpreter::run_saga_effect(effect, &mut ictx).await,
            InterpretOutcome::DlqFailed
        ) {
            tracing::error!(
                saga_id = self.saga_id.as_str(),
                tell_id,
                "saga DLQ append failed for PersistFailed (tell-failure hook); \
                 stopping for supervised restart"
            );
            let _ = ctx.stop_self().await;
        }
    }
}

impl<S: Saga> Process for SagaProcess<S> {
    async fn on_start(&mut self, ctx: &mut ProcessContext<Self>) {
        let factory_ref = self.crash_restart_factory.as_ref().map(|f| f.as_ref());
        let outcome = match replay::replay_and_redispatch(
            &self.saga_id,
            &mut self.state,
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
            // terminated saga is not revived.
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
        // What earlier incarnations consumed is theirs to hand over: it decides
        // both which redelivered records this one drops and where its
        // subscription resumes.
        self.consumed = outcome.consumed;

        // A saga whose stream carries a durable `Ended` marker terminated in a
        // previous incarnation, so this one starts in the same drained
        // lifecycle an in-process `End` leaves behind: `Saga::handle` can never
        // run again, and anything still routed here becomes a dead letter.
        if outcome.ended {
            self.lifecycle = Lifecycle::Draining;
            tracing::info!(
                saga_id = self.saga_id.as_str(),
                "saga already terminated (Ended marker present); skipping subscription"
            );
            // Owning the subscription and declining to wire it means nothing
            // can reach this process again, so the inert incarnation releases
            // its resources by stopping.  Under a manager the shared
            // subscription keeps delivering here and each arrival must be
            // recorded on this saga's own stream, so the instance instead stays
            // resident until the manager passivates it — and its dead letters
            // still need the poller spawned below to reach the subscriber.
            if self.owns_its_subscriptions() {
                if let Err(e) = ctx.stop_self().await {
                    tracing::warn!(error = %e, "terminated saga stop_self failed");
                }
                return;
            }
        } else {
            // Re-register timers that were still pending at the previous
            // incarnation's stop: unfired, uncancelled schedules found on the
            // saga's own stream.
            self.reregister_active_schedules(outcome.active_schedules, ctx)
                .await;

            // One poller per declared source, each resuming from its own
            // position. Resuming past what the journal says *that source*
            // consumed is what frees a restart from re-reading the whole
            // upstream history; the configured position still holds as a floor.
            // Keeping the positions per source is what stops one source's
            // progress from skipping another's unread records.
            for upstream in self.upstream.clone() {
                let cursor = self.consumed.resume(upstream.cursor, &upstream.source);
                upstream.config.spawn_child(ctx, cursor).await;
            }
        }

        // Spawn the DLQ direct-poller as a child of
        // this saga process so that when the saga stops the runtime
        // cascade-stops the DLQ poller automatically.  Without child binding
        // the poller would outlive the saga as long as the subscriber is alive.
        if let Some(ref dlq) = self.dlq_child_spawn {
            dlq.spawn_as_child(ctx, &self.saga_id, &self.store).await;
        }
    }

    async fn on_stop(&mut self, _ctx: &mut ProcessContext<Self>) {
        // Saga lifecycle teardown.  A deadline belongs to the saga instance
        // rather than to this incarnation, so releasing one here is right only
        // where the firing is addressed to this process: once it is stopped,
        // such a firing has nowhere to land and the entry would sit in the
        // resident scheduler for nothing.  An instance a manager fronts is
        // addressed through that manager, which restores it — so its deadlines
        // must stay armed across a passivation that may well be shorter than
        // the deadline itself.
        //
        // That exception holds only while the saga is still `Running`: once
        // `SagaEffect::End` has landed (or replay found the `Ended` marker
        // from a previous incarnation), `lifecycle` is `Draining` and there is
        // no future activation this deadline could still serve — restoring the
        // instance to hand it a firing only produces the dead letter
        // the deadline arm can never run to settle any other way. So a draining
        // stop always releases, manager-fronted or not.
        //
        // Either way this is an in-memory release only — no `Cancelled` marker
        // is persisted, so a non-terminal later incarnation replays and
        // re-registers any still unfired schedule.
        if self.delivery.restores_the_instance() && matches!(self.lifecycle, Lifecycle::Running) {
            return;
        }
        if let Some(timers) = self.timers() {
            timers.cancel_all().await;
        }
    }
}

impl<S: Saga> Receive<UpstreamMessage<S::Subscribe>> for SagaProcess<S> {
    type Response = ();
    /// Returning `Err(SagaUpstreamHandlerError)` signals the `DirectPoller`
    /// (via `ask()`) to NOT advance the upstream cursor, keeping the saga
    /// state consistent with what was actually processed.
    type Error = SagaUpstreamHandlerError;

    async fn recv(
        &mut self,
        msg: UpstreamMessage<S::Subscribe>,
        ctx: &mut ProcessContext<Self>,
    ) -> Result<(), SagaUpstreamHandlerError> {
        let UpstreamMessage {
            aggregate_id,
            sequence,
            source: declared_source,
            input,
            raw_payload,
        } = msg;

        // Correlation is decided before the lifecycle is consulted: an event
        // belonging to another instance is none of this saga's business even
        // while it drains, so it must not become a dead letter here.
        let Some(target_id) = S::correlate(&input) else {
            return Ok(());
        };
        if target_id != self.saga_id {
            return Ok(());
        }

        // A record already consumed — whether by a `Saga::handle` decision or
        // by a dead-letter append that settled it without one — must not
        // reach `Saga::handle` again, and delivery is at-least-once, so the
        // same record can arrive again — after a restart, or while this
        // incarnation is still running.  It is dropped here, before anything
        // else looks at it: `Saga::handle` must not see an input it has
        // already consumed, and the drop must not spend the lifecycle's dead
        // letter or the one report a failed tell is owed.
        if self
            .consumed
            .covers(aggregate_id.as_str(), &declared_source, sequence)
        {
            tracing::debug!(
                saga_id = self.saga_id.as_str(),
                upstream_key = aggregate_id.as_str(),
                upstream_sequence = sequence,
                "saga dropped a redelivered observed record it has already consumed"
            );
            return Ok(());
        }

        let source = SourceContext {
            aggregate_id: aggregate_id.clone(),
            upstream_sequence: sequence,
        };
        // The position every settle path below writes: the stream, the declared
        // source that read the record, and its sequence. Built once so the three
        // paths cannot drift on what they claim to have settled.
        let input_position = ConsumedPositionEvent {
            source: aggregate_id.clone(),
            source_event_type: declared_source,
            sequence,
        };

        // Once `End` has been interpreted and the durable
        // `Ended` marker persisted, the lifecycle transitions to `Draining`
        // while in-flight tells settle.  Upstream events routed here during
        // draining must not reach `Saga::handle` — doing so could produce
        // additional effects that violate the terminal lifecycle contract.  The
        // dropped message is recorded as a dead letter immediately, with its
        // raw wire payload preserved.
        if matches!(self.lifecycle, Lifecycle::Draining) {
            // This call settles the record with `Ok`, so its position must
            // become durable in the same append as the dead letter — see
            // `record_dead_letter`'s doc comment.
            if !self
                .record_dead_letter(
                    SagaFailure::EndedSagaReceivedMessage {
                        message: raw_payload,
                    },
                    source,
                    Some(input_position),
                )
                .await
            {
                // Return Err so the DirectPoller (via ask()) does NOT
                // advance the upstream cursor — the runtime will stop this
                // process via handler-failure supervision.
                tracing::error!(
                    saga_id = self.saga_id.as_str(),
                    "saga DLQ append failed for EndedSagaReceivedMessage; signalling upstream handler error"
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
        let effect = match self.state.handle(input, &mut saga_ctx).await {
            Ok(effect) => effect,
            Err(e) => {
                tracing::warn!(error = %e, "saga handle failed");
                // This call settles the record with `Ok`, so its position
                // must become durable in the same append as the dead letter —
                // see `record_dead_letter`'s doc comment.
                if !self
                    .record_dead_letter(
                        SagaFailure::HandleFailed {
                            error: e.to_string(),
                        },
                        source,
                        Some(input_position),
                    )
                    .await
                {
                    // Return Err so the DirectPoller (via ask()) does NOT
                    // advance the upstream cursor — the runtime will stop this
                    // process via handler-failure supervision.
                    tracing::error!(
                        saga_id = self.saga_id.as_str(),
                        "saga DLQ append failed for HandleFailed; signalling upstream handler error"
                    );
                    return Err(SagaUpstreamHandlerError);
                }
                return Ok(());
            }
        };

        // Hand the upstream record's position to the interpreter as a
        // `ConsumedPositionEvent`; the interpreter's effect interpretation
        // (see `interpreter::run_saga_effect`) owns whether that position is
        // attached to an append at all, and if so, to which one. For a
        // `Sequence`, only the closing append carries it, and only once every
        // preceding append in the batch has landed — so a decision can become
        // durable without its input's consumed position becoming durable too.
        let mut ictx = self.interpreter_ctx(ctx, Some(input_position));
        // If the interpreter signals DlqFailed (PersistFailed DLQ
        // append also failed), propagate Err so the DirectPoller (via ask())
        // does NOT advance the upstream cursor.
        if matches!(
            interpreter::run_saga_effect(effect, &mut ictx).await,
            InterpretOutcome::DlqFailed
        ) {
            tracing::error!(
                saga_id = self.saga_id.as_str(),
                "saga DLQ append failed for PersistFailed (upstream handler); \
                 signalling upstream handler error"
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
        // Mirror the observed-record guard — a firing must also be dropped while
        // draining, so the deadline arm of `Saga::handle` cannot produce further
        // effects after the terminal `Ended` marker has been written.  The
        // dropped firing is recorded as a dead letter (its raw payload retained).
        if matches!(self.lifecycle, Lifecycle::Draining) {
            if !self
                .record_dead_letter(
                    SagaFailure::EndedSagaReceivedMessage {
                        message: msg.payload.clone(),
                    },
                    SourceContext::without_upstream(),
                    None,
                )
                .await
            {
                // DLQ append failed — stop so the timer firing is not
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
        // A firing is read back as the alphabet's own deadline variant, through
        // the codec the message type named — the same one that wrote it into the
        // schedule fact.
        //
        // `READ` is derived from `S::Subscribe::Deadline` (see
        // `DeclaresDeadline`), not a fact the alphabet sets on its own, so it is
        // `None` only where `Deadline` itself is `NoDeadline` — a schedule fact
        // written by an incarnation that declared a deadline, fired at one that
        // does not. There is no arm to deliver it to, and no retry that would
        // make one appear, so it is recorded as the unreadable payload it is.
        let read_back = match <<S::Subscribe as SagaSubscribe>::Deadline as DeclaresDeadline<
            S::Subscribe,
        >>::READ
        {
            Some(read) => read(&payload).map_err(|e| e.to_string()),
            None => Err(
                "this saga's alphabet declares no deadline variant, so a firing \
                 for it has no arm to be delivered to"
                    .to_owned(),
            ),
        };
        let input = match read_back {
            Ok(input) => input,
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    timer = name.as_str(),
                    "saga could not read a deadline firing; dropping it"
                );
                if !self
                    .record_dead_letter(
                        SagaFailure::DecodeFailed { error },
                        SourceContext::without_upstream(),
                        None,
                    )
                    .await
                {
                    // DLQ append failed — stop so the timer firing is not
                    // treated as processed; a supervised restart will retry.
                    tracing::error!(
                        saga_id = self.saga_id.as_str(),
                        "saga DLQ append failed for DecodeFailed (deadline); stopping for supervised restart"
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
        let effect = match self.state.handle(input, &mut saga_ctx).await {
            Ok(effect) => effect,
            Err(e) => {
                // The failure kind names the entry the input arrived at, not the
                // method that ran: a deadline that could not be handled is
                // `ScheduledFailed` whether or not it shares `handle` with the
                // observed records, which is what keeps an `EnqueuePolicy`
                // written against the kind meaning the same thing.
                tracing::warn!(error = %e, "saga failed to handle a deadline firing");
                if !self
                    .record_dead_letter(
                        SagaFailure::ScheduledFailed {
                            error: e.to_string(),
                        },
                        SourceContext::without_upstream(),
                        None,
                    )
                    .await
                {
                    // DLQ append failed — stop so the timer firing is not
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
        // if the deadline arm re-sets under the same `TimerName` the event
        // stream order becomes:
        //
        //   … Scheduled → Fired → (new) Scheduled
        //
        // Replay folds these left-to-right per name:
        //   insert(name) → remove(name) → insert(name)  ⟹  net: active (correct)
        //
        // The opposite order (… Scheduled → (new) Scheduled → Fired) would
        // cause replay to remove the new schedule when it processes the Fired
        // marker, breaking timer cancellation and re-registration.
        //
        // Best-effort once: an append failure is tolerated (at-least-once
        // delivery permits a replay re-fire).
        let fired_event = ScheduleEvent::Fired {
            token: ScheduleToken::new(self.saga_id.clone(), name.clone()),
            fired_at_unix_millis: jiff::Timestamp::now().as_millisecond(),
        };
        // A deadline firing is not an observed record, so it consumes nothing.
        if scheduler::append_schedule_marker(
            &self.store,
            &self.saga_id,
            &mut self.sequence,
            fired_event,
            jiff::Timestamp::now(),
            None,
            &mut self.consumed,
        )
        .await
        {
            // The view follows the journal: the deadline leaves it when its
            // `fired` marker is durable, and not when the firing arrived. An
            // append that did not land leaves the deadline pending, which is
            // what a replay would also conclude from the stream.
            deadline::forget_pending(&self.pending_deadlines, &name);
        }

        let mut ictx = self.interpreter_ctx(ctx, None);
        // FireScheduled is not delivered via DirectPoller so there is no
        // upstream cursor to withhold; call stop_self() to ensure the process
        // does not silently continue when a PersistFailed DLQ append also fails.
        if matches!(
            interpreter::run_saga_effect(effect, &mut ictx).await,
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
                TellOutcome::Failed => self.settle_failed_tell(msg.tell_id, ctx).await,
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

impl<S: Saga> Receive<RecordDisposition> for SagaProcess<S> {
    /// The append's outcome is the *reply*, not the handler's result.
    ///
    /// A saga instance's default supervision strategy is `Stop`, so returning
    /// `Err` here would let an operator's failed bookkeeping write take down a
    /// perfectly healthy saga.  The operator asked a question; they get the
    /// answer, and the saga carries on either way.
    type Response = Result<(), AppendError>;
    type Error = std::convert::Infallible;

    async fn recv(
        &mut self,
        msg: RecordDisposition,
        _ctx: &mut ProcessContext<Self>,
    ) -> Result<Result<(), AppendError>, std::convert::Infallible> {
        // This instance owns its stream's next sequence, and this handler runs
        // on the same mailbox as everything else that appends to it — which is
        // what makes the marker's position uncontended rather than guessed.
        //
        // No lifecycle guard: a dead letter left behind by a saga that has
        // since ended still has to be settleable.
        let candidate = self.sequence + 1;
        let appending = msg.marker.appending(candidate, jiff::Timestamp::now());
        match self
            .store
            .append(self.saga_id.as_str(), vec![appending])
            .await
        {
            Ok(_) => {
                // Advance only on success, so a refused marker leaves no gap
                // for the saga's own next append to fall into.
                self.sequence = candidate;
                report_system_event!(
                    DeadLetterDispositionEvent::EVENT_TYPE,
                    msg.marker.arm(),
                    self.saga_id.as_str(),
                    dead_letter_sequence = msg.marker.dead_letter_sequence,
                );
                Ok(Ok(()))
            }
            Err(e) => {
                tracing::warn!(
                    saga_id = self.saga_id.as_str(),
                    error = %e,
                    "saga dead letter disposition append failed; reported to the caller"
                );
                Ok(Err(e))
            }
        }
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

    use nitinol_eventsource::codec::Codec;
    use nitinol_eventsource::test_helpers::MockAggregateProxy;
    use nitinol_eventsource::{Aggregate, Decider, Decision, Event};
    use nitinol_persistence::{EventType, Family, TypeName};

    #[derive(Clone)]
    struct Noted;

    struct UnitCodec;

    impl Codec<Noted> for UnitCodec {
        type Error = std::convert::Infallible;

        fn encode(_: &Noted) -> Result<Bytes, Self::Error> {
            Ok(Bytes::new())
        }

        fn decode(_: &[u8]) -> Result<Noted, Self::Error> {
            Ok(Noted)
        }
    }

    impl Event for Noted {
        const EVENT_TYPE: EventType = EventType::new(
            Family::new("saga_process_unit_test"),
            TypeName::new("Noted"),
        );

        type Codec = UnitCodec;
    }

    #[derive(Default)]
    struct NotingAggregate;

    impl Aggregate for NotingAggregate {
        type Event = Noted;

        fn apply(&mut self, _event: Noted) {}
    }

    #[derive(Clone)]
    struct Note;

    impl Decider<Note> for NotingAggregate {
        type Output = ();
        type Rejection = std::convert::Infallible;

        fn decide(&self, _cmd: Note) -> Decision<Noted, (), Self::Rejection> {
            Decision::persist(Vec::new()).output(())
        }
    }

    /// A tell to hold in a [`TellState`].  What it would send is immaterial
    /// here: these tests read which states still hold one.
    fn intent() -> TellIntent {
        TellIntent::new::<NotingAggregate, Note, _>(
            MockAggregateProxy::<NotingAggregate>::new(),
            Note,
        )
    }

    /// Given each state a tell can be in, When it is asked for the intent it
    /// can be re-dispatched from, Then the two states that still hold one
    /// answer with it and the one that has given it up answers with nothing.
    ///
    /// A tell whose terminal marker failed to append is still a tell that was
    /// sent, so replay re-dispatches it; a failed one has already been recorded
    /// as failed, and re-sending it would repeat a delivery the stream says is
    /// over.
    #[test]
    fn only_a_tell_that_still_holds_its_intent_answers_with_one() {
        assert!(
            TellState::Pending(intent()).intent().is_some(),
            "a pending tell must still offer the intent it is waiting on"
        );
        assert!(
            TellState::AppendFailed(intent()).intent().is_some(),
            "a tell whose terminal marker did not land must still offer its intent"
        );
        assert!(
            TellState::Failed.intent().is_none(),
            "a failed tell has no intent left to re-dispatch"
        );
    }

    /// Given each state a tell can be in, When it is asked whether it is still
    /// in flight, Then only the pending one is.
    ///
    /// This is what a draining saga waits on before it stops, so counting a
    /// tell that has already reached its outcome would keep the saga alive
    /// with nothing left to wait for.
    #[test]
    fn only_a_pending_tell_is_in_flight() {
        assert!(
            TellState::Pending(intent()).is_in_flight(),
            "a pending tell is out with its outcome unknown"
        );
        assert!(
            !TellState::AppendFailed(intent()).is_in_flight(),
            "a tell whose outcome is known is not in flight, marker or no marker"
        );
        assert!(
            !TellState::Failed.is_in_flight(),
            "a failed tell has reached its outcome"
        );
    }

    /// The count a draining saga reads is the number of pending tells, and
    /// nothing else in the registry contributes to it.
    #[test]
    fn in_flight_count_counts_the_pending_tells_alone() {
        let mut tell_states = HashMap::new();
        tell_states.insert(1, TellState::Pending(intent()));
        tell_states.insert(2, TellState::AppendFailed(intent()));
        tell_states.insert(3, TellState::Failed);
        tell_states.insert(4, TellState::Pending(intent()));

        assert_eq!(
            in_flight_count(&tell_states),
            2,
            "two of the four tells are pending, so two are in flight"
        );
    }

    #[tokio::test]
    async fn append_terminal_does_not_advance_sequence_on_store_failure() {
        let store: Arc<dyn EventStore> = Arc::new(FailOnceStore::new());
        let saga_id = SagaId::new("sequence-skip-regression");
        let mut sequence: u64 = 0;

        let ok =
            append_terminal_and_claim(&store, &saga_id, &mut sequence, TellOutcome::Acked, 1).await;
        assert!(!ok, "must return false when EventStore::append fails");
        assert_eq!(sequence, 0, "sequence must not advance on store failure");

        let ok =
            append_terminal_and_claim(&store, &saga_id, &mut sequence, TellOutcome::Acked, 1).await;
        assert!(ok, "must return true when EventStore::append succeeds");
        assert_eq!(
            sequence, 1,
            "sequence must be 1 (no gap from the failed attempt)"
        );
    }
}
