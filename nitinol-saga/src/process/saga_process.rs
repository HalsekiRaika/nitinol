use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use nitinol_eventsource::{DurableSubscription, SequenceCursor};
use nitinol_persistence::store::EventStore;
use nitinol_persistence::{AggregateId, LoadedEvent};
use nitinol_runtime::process::{Process, ProcessContext, Receive};

use crate::components::{deadline, PendingDeadlineCell};
use crate::consumed::{self, ConsumedPositionEvent, ConsumedPositions, PositionSource};
use crate::context::SagaContext;
use crate::error::SagaUpstreamHandlerError;
use crate::id::SagaId;
use crate::journal::ActiveSchedule;
use crate::process::instance::ScheduleDelivery;
use crate::process::interpreter::{self, InterpretOutcome, InterpreterCtx};
use crate::process::replay;
use crate::retry_policy::RetryPolicy;
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
            let aggregate_id = AggregateId::new(loaded.stream_key);
            let sequence = loaded.sequence;
            match decode(&loaded.payload) {
                Ok(input) => Some(UpstreamMessage {
                    aggregate_id,
                    sequence,
                    source: of_source.clone(),
                    input,
                }),
                Err(e) => {
                    // Reported and skipped, which is the whole of the handling:
                    // answering `None` advances this source's cursor past the
                    // record. A codec that refused these bytes once refuses them
                    // every time, and with no typed input there is no instance
                    // to attribute the failure to — so there is nothing a
                    // retry or a held cursor could recover.
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

/// Whether this saga instance can still reach [`Saga::handle`].
///
/// `Ended` is set when `SagaEffect::End`'s durable fact lands, and when replay
/// finds that fact from an earlier incarnation.  It is final: nothing moves an
/// instance back to `Running`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Lifecycle {
    Running,
    Ended,
}

pub struct SagaProcess<S: Saga> {
    pub(crate) state: S,
    pub(crate) saga_id: SagaId,
    pub(crate) store: Arc<dyn EventStore>,
    pub(crate) sequence: u64,
    /// How far each upstream source this saga reads has been consumed.
    ///
    /// Restored by replay from the saga's own stream and advanced by every
    /// batch that lands, so a record already consumed — whether by a
    /// [`Saga::handle`] decision or by the position-only append that settles
    /// an input `handle` refused — is dropped before it reaches
    /// [`Saga::handle`] again, whether the position was folded out of the
    /// journal or written by this incarnation.
    pub(crate) consumed: ConsumedPositions,
    pub(crate) retry_policy: RetryPolicy,
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
    /// The identity the schedule fact persisted `payload` under — checked
    /// against what the firing alphabet currently declares before the bytes
    /// are decoded, so a stale or disagreeing declaration is refused rather
    /// than guessed at.
    pub(crate) message_type: String,
    pub(crate) message_codec: String,
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
            let dispatch = self.delivery.dispatch(
                &self.saga_id,
                ctx,
                sched.name.clone(),
                sched.message_type,
                sched.message_codec,
                sched.payload,
            );
            timers.start_single(sched.name, after, dispatch).await;
        }
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
            lifecycle: &mut self.lifecycle,
            scheduler: self.scheduler.clone(),
            pending_deadlines: Arc::clone(&self.pending_deadlines),
            landed_schedules: Vec::new(),
            delivery: &self.delivery,
            consumed: &mut self.consumed,
            input_position,
        }
    }

    /// Settle a deadline firing: append the `fired` marker that closes it out,
    /// and drop it from the pending view once that marker is durable.
    ///
    /// This marker is to a firing what the consumed position is to an observed
    /// record — the row that says the input is dealt with and must not come
    /// round again.  A firing was addressed to this instance rather than read
    /// from a stream, so it carries no position and this is the only row that
    /// can say so.  Which is why it is appended *after* whatever the firing
    /// decided: written first, a decision that never became durable would leave
    /// behind a deadline replay folds out of the journal for good — the firing
    /// lost, the decision with it.
    ///
    /// Appended once, with no retry.  An append that did not land leaves the
    /// schedule fact unfired on the stream, so the deadline stays pending and a
    /// later activation re-arms and re-delivers it: a repeated firing, which
    /// [`Saga::handle`] is already obliged to tolerate, in place of a lost one.
    async fn settle_fired(&mut self, name: &TimerName) {
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
            // `fired` marker is durable, and not when the firing arrived.
            deadline::forget_pending(&self.pending_deadlines, name);
            return;
        }
        tracing::error!(
            saga_id = self.saga_id.as_str(),
            timer = name.as_str(),
            "saga could not settle a deadline firing; the schedule fact stays \
             unfired, so the deadline is still pending and a later activation \
             re-arms and re-delivers it"
        );
    }
}

impl<S: Saga> Process for SagaProcess<S> {
    async fn on_start(&mut self, ctx: &mut ProcessContext<Self>) {
        let outcome = match replay::replay(
            &self.saga_id,
            &mut self.state,
            &self.store,
            &mut self.sequence,
        )
        .await
        {
            Ok(o) => o,
            // The stream could not be read to its end — the store refused the
            // load, or a row belongs to no declaration this version owns.
            // Fail safe: stop the process without subscribing, so a saga whose
            // own history cannot be read never acts on it.
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
        // What earlier incarnations consumed is theirs to hand over: it decides
        // both which redelivered records this one drops and where its
        // subscription resumes.
        self.consumed = outcome.consumed;

        // A saga whose stream carries a durable `Ended` fact terminated in a
        // previous incarnation, so this one starts in the same terminal
        // lifecycle an in-process `End` leaves behind: `Saga::handle` can never
        // run again, and anything still routed here is dropped.
        if outcome.ended {
            self.lifecycle = Lifecycle::Ended;
            tracing::info!(
                saga_id = self.saga_id.as_str(),
                "saga already terminated (Ended fact present); skipping subscription"
            );
            // Owning the subscription and declining to wire it means nothing
            // can reach this process again, so the inert incarnation releases
            // its resources by stopping.  Under a manager the shared
            // subscription keeps delivering here, and the manager reads this
            // instance's answer to decide whether that source's cursor may
            // advance — so the instance stays resident to accept and drop each
            // arrival until the manager passivates it.  Stopping instead would
            // make every such arrival unanswerable and hold the shared cursor
            // behind an instance that will never act again.
            if self.owns_its_subscriptions() {
                if let Err(e) = ctx.stop_self().await {
                    tracing::warn!(error = %e, "terminated saga stop_self failed");
                }
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
        // `SagaEffect::End` has landed (or replay found the `Ended` fact from a
        // previous incarnation), `lifecycle` is `Ended` and there is no
        // future activation this deadline could still serve — restoring the
        // instance to hand it a firing only wakes a saga whose deadline arm can
        // never run again. So a terminal stop always releases, manager-fronted
        // or not.
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
        } = msg;

        // Correlation is decided before the lifecycle is consulted: an event
        // belonging to another instance is none of this saga's business, ended
        // or not.
        let Some(target_id) = S::correlate(&input) else {
            return Ok(());
        };
        if target_id != self.saga_id {
            return Ok(());
        }

        // A record already consumed — whether by a `Saga::handle` decision or
        // by the position-only append that settled one `handle` refused — must
        // not reach `Saga::handle` again, and delivery is at-least-once, so the
        // same record can arrive again — after a restart, or while this
        // incarnation is still running.  It is dropped here, before anything
        // else looks at it.
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

        // The position each settle path below writes: the stream, the declared
        // source that read the record, and its sequence. Built once so the
        // paths cannot drift on what they claim to have settled.
        let input_position = ConsumedPositionEvent {
            source: aggregate_id.clone(),
            source_event_type: declared_source,
            sequence,
        };

        // Once `End` has been interpreted and the durable `Ended` fact
        // persisted, the lifecycle is `Ended`.  Upstream events routed here
        // afterwards must not reach `Saga::handle` — doing so could produce
        // additional effects that violate the terminal lifecycle contract — so
        // the arrival is reported and dropped.  Nothing durable is written for
        // it: a terminated saga records no further facts, and a position would
        // claim an input a `handle` never saw.  The record is still answered
        // with `Ok` so the cursor moves past it; holding it would starve every
        // instance behind this one on a shared subscription.
        if matches!(self.lifecycle, Lifecycle::Ended) {
            tracing::warn!(
                saga_id = self.saga_id.as_str(),
                upstream_key = aggregate_id.as_str(),
                upstream_sequence = sequence,
                "saga has terminated; dropping an observed record routed to it"
            );
            return Ok(());
        }

        let current_sequence = self.sequence;
        let mut saga_ctx = SagaContext::new(
            self.saga_id.clone(),
            current_sequence,
            aggregate_id.clone(),
            sequence,
            jiff::Timestamp::now(),
        );
        let effect = match self.state.handle(input, &mut saga_ctx).await {
            Ok(effect) => effect,
            Err(e) => {
                // The refusal produces no decision, so the position is the only
                // row the input leaves — it was seen, it produced nothing, and
                // it must not come round again.  The error itself reaches an
                // operator here and nowhere else.
                tracing::error!(
                    error = %e,
                    saga_id = self.saga_id.as_str(),
                    upstream_key = aggregate_id.as_str(),
                    upstream_sequence = sequence,
                    "saga refused an observed record; settling it by its position alone"
                );
                if !consumed::settle_with_position(
                    &self.store,
                    &self.saga_id,
                    &mut self.sequence,
                    &input_position,
                    &mut self.consumed,
                )
                .await
                {
                    // Return Err so the DirectPoller (via ask()) does NOT
                    // advance the upstream cursor: with no position the input is
                    // unconsumed, and a redelivery must reach `handle` again.
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
        // An append the effect required never landed, so no position was
        // written and this input is not consumed.  Reported and answered with
        // `Ok`: raising it would put this process through handler-failure
        // supervision, and a saga that stops here is exactly what must not
        // happen — staying alive is what lets the *next* input retry the
        // decision, including an `End` whose `Ended` fact did not land. Nothing
        // durable claims the record either, so a restart re-reads it from
        // upstream: the subscription resumes from the position the journal
        // records for this source, which this failure left where it was.
        if matches!(
            interpreter::run_saga_effect(effect, &mut ictx).await,
            InterpretOutcome::AppendFailed
        ) {
            tracing::error!(
                saga_id = self.saga_id.as_str(),
                upstream_key = aggregate_id.as_str(),
                upstream_sequence = sequence,
                "saga could not persist what it decided from an observed record; \
                 the record stays unconsumed, so a restart decides it again"
            );
        }
        Ok(())
    }
}

/// A deadline of this instance has come due.
///
/// The firing settles behind its decision, the way every other input a saga
/// takes does: the `fired` marker that closes the deadline out is appended only
/// once what the firing decided is durable — see `SagaProcess::settle_fired` —
/// and is withheld entirely where there is nothing left to close:
///
/// - the decision never became durable, so the deadline stays pending and a
///   later activation re-arms and re-delivers it;
/// - the effect re-set this very deadline and that fact landed, so the new
///   schedule supersedes the old one by name on its own;
/// - the saga terminated on this firing, so it re-arms nothing ever again.
///
/// A firing that decides nothing at all — refused, unreadable, or
/// `SagaEffect::None` — is settled by the marker alone, which is the discipline
/// the upstream path settles a refused record by its position alone with.
impl<S: Saga> Receive<FireScheduled> for SagaProcess<S> {
    type Response = ();
    type Error = std::convert::Infallible;

    async fn recv(
        &mut self,
        msg: FireScheduled,
        ctx: &mut ProcessContext<Self>,
    ) -> Result<(), std::convert::Infallible> {
        // Mirror the observed-record guard — a firing must also be dropped once
        // the saga has ended, so the deadline arm of `Saga::handle` cannot
        // produce further effects after the terminal `Ended` fact is written.
        if matches!(self.lifecycle, Lifecycle::Ended) {
            tracing::warn!(
                saga_id = self.saga_id.as_str(),
                timer = msg.name.as_str(),
                "saga has terminated; dropping a deadline firing addressed to it"
            );
            return Ok(());
        }

        let FireScheduled {
            name,
            message_type,
            message_codec,
            payload,
        } = msg;
        // A firing is read back as the alphabet's own deadline variant, through
        // the codec the message type named — the same one that wrote it into the
        // schedule fact — after checking the fact's own identity against what
        // this alphabet currently declares: a stale or disagreeing declaration
        // is refused rather than guessed at (see `ReadDeadlineError`).
        //
        // `READ` is derived from `S::Subscribe::Deadline` (see
        // `DeclaresDeadline`), not a fact the alphabet sets on its own, so it is
        // `None` only where `Deadline` itself is `NoDeadline` — a schedule fact
        // written by an incarnation that declared a deadline, fired at one that
        // does not. There is no arm to deliver it to, and no retry that would
        // make one appear, so it is reported and dropped.
        let read_back = match <<S::Subscribe as SagaSubscribe>::Deadline as DeclaresDeadline<
            S::Subscribe,
        >>::READ
        {
            Some(read) => read(&message_type, &message_codec, &payload).map_err(|e| e.to_string()),
            None => Err(
                "this saga's alphabet declares no deadline variant, so a firing \
                 for it has no arm to be delivered to"
                    .to_owned(),
            ),
        };
        let input = match read_back {
            Ok(input) => input,
            Err(error) => {
                // Nothing readable reached `handle`, so there is no decision to
                // settle behind and the marker is the whole of what this firing
                // leaves — the discipline an undecodable observed record is read
                // past by.  The schedule fact names an identity this alphabet
                // does not declare, which no restart changes: left unsettled,
                // the deadline would be re-armed by every later activation only
                // to repeat this report.
                tracing::error!(
                    error = %error,
                    saga_id = self.saga_id.as_str(),
                    timer = name.as_str(),
                    "saga could not read a deadline firing; settling it by its \
                     `fired` marker alone"
                );
                self.settle_fired(&name).await;
                return Ok(());
            }
        };

        let current_sequence = self.sequence;
        let mut saga_ctx = SagaContext::new(
            self.saga_id.clone(),
            current_sequence,
            AggregateId::new(""),
            0,
            jiff::Timestamp::now(),
        );
        let effect = match self.state.handle(input, &mut saga_ctx).await {
            Ok(effect) => effect,
            Err(e) => {
                // A refusal produces no decision, so the marker that closes the
                // deadline out is the only row the firing leaves — the
                // discipline an observed record `handle` refused is settled by,
                // where its position alone is that row.  The error itself
                // reaches an operator here and nowhere else.
                tracing::error!(
                    error = %e,
                    saga_id = self.saga_id.as_str(),
                    timer = name.as_str(),
                    "saga refused a deadline firing; settling it by its `fired` \
                     marker alone"
                );
                self.settle_fired(&name).await;
                return Ok(());
            }
        };

        // What the firing decided goes to the store first, and only what landed
        // decides how the firing is settled: `landed_schedules` names the timers
        // whose `Scheduled` marker became durable while this effect ran, so a
        // reset the store refused is not mistaken for one that took effect.
        let (outcome, reset_this_deadline) = {
            let mut ictx = self.interpreter_ctx(ctx, None);
            let outcome = interpreter::run_saga_effect(effect, &mut ictx).await;
            let reset_this_deadline = ictx.landed_schedules.contains(&name);
            (outcome, reset_this_deadline)
        };

        match outcome {
            // A firing does not arrive through a `DirectPoller`, so there is no
            // upstream cursor to withhold and no redelivery to wait for.  The
            // settlement is withheld instead: with no `fired` marker the
            // schedule fact stays unfired on the stream, so a later activation
            // re-arms the deadline — at once, its due time being past — and
            // decides it again.  Nothing is retried while this incarnation
            // lives.
            InterpretOutcome::AppendFailed => {
                tracing::error!(
                    saga_id = self.saga_id.as_str(),
                    timer = name.as_str(),
                    "saga could not persist what it decided from a deadline \
                     firing; the deadline stays pending, so a later activation \
                     re-arms and re-delivers it"
                );
            }
            // `End`'s durable fact has landed and the process is stopping.  A
            // terminated saga re-arms nothing, so there is no later activation
            // for a settlement to keep this deadline from.
            InterpretOutcome::Stop => {}
            // The effect re-set this very deadline and that fact is durable.
            // Folding the journal replaces a schedule by name, so the new fact
            // supersedes the one that fired all on its own — while a `fired`
            // marker written after it would be folded by the same name and
            // remove the deadline the saga has just re-armed.
            InterpretOutcome::Continue if reset_this_deadline => {}
            InterpretOutcome::Continue => self.settle_fired(&name).await,
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
    use nitinol_persistence::{
        AppendOutcome, AppendingEvent, EventType, Family, LoadQuery, TypeName,
    };

    use crate::consumed::{self, ConsumedPositionEvent, ConsumedPositions, PositionSource};

    /// Refuses the first append and accepts every one after it, so a test can
    /// observe both halves of "the sequence advances only on success".
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

    const OBSERVED: EventType = EventType::new(
        Family::new("saga_process_unit_test"),
        TypeName::new("Observed"),
    );

    fn position(sequence: u64) -> ConsumedPositionEvent {
        ConsumedPositionEvent {
            source: AggregateId::new("order-stream"),
            source_event_type: PositionSource::of(OBSERVED),
            sequence,
        }
    }

    /// Given a store that refuses the first append, When a refused input is
    /// settled by its position alone, Then the failed attempt leaves the saga's
    /// sequence where it was and the input stays unconsumed — so a redelivery
    /// can reach `handle` again — and the next attempt claims the sequence with
    /// no gap.
    #[tokio::test]
    async fn settling_a_refused_input_claims_its_sequence_only_once_the_append_lands() {
        let store: Arc<dyn EventStore> = Arc::new(FailOnceStore::new());
        let saga_id = SagaId::new("position-only-settle");
        let mut sequence: u64 = 0;
        let mut consumed = ConsumedPositions::default();

        let settled = consumed::settle_with_position(
            &store,
            &saga_id,
            &mut sequence,
            &position(4),
            &mut consumed,
        )
        .await;

        assert!(
            !settled,
            "a store that refused the append has not settled the input"
        );
        assert_eq!(sequence, 0, "sequence must not advance on store failure");
        assert!(
            !consumed.covers("order-stream", &PositionSource::of(OBSERVED), 4),
            "an input whose position never landed must stay unconsumed, so a \
             redelivery reaches `handle` again"
        );

        let settled = consumed::settle_with_position(
            &store,
            &saga_id,
            &mut sequence,
            &position(4),
            &mut consumed,
        )
        .await;

        assert!(settled, "the retried append lands, settling the input");
        assert_eq!(
            sequence, 1,
            "the position claims the next sequence, with no gap from the failed attempt"
        );
        assert!(
            consumed.covers("order-stream", &PositionSource::of(OBSERVED), 4),
            "the durable position is what stops the record coming round again"
        );
    }
}
