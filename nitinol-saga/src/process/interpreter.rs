//! Interpreter for [`SagaEffect`].

use std::borrow::Borrow;

use futures_core::future::BoxFuture;
use nitinol_eventsource::{EventAppending, SystemEvent};
use nitinol_persistence::store::Journal;
use nitinol_persistence::AppendingEvent;
use nitinol_runtime::process::ProcessContext;

use crate::components::{deadline, PendingDeadlines};
use crate::consumed::{
    append_consumed_position, finalize_consumed_position, ConsumedPositionEvent, ConsumedPositions,
};
use crate::effect::ScheduleSpec;
use crate::id::SagaId;
use crate::lifecycle;
use crate::observability::report_system_event;
use crate::process::instance::ScheduleDelivery;
use crate::process::saga_process::{Lifecycle, SagaProcess};
use crate::retry_policy::RetryPolicy;
use crate::saga::Saga;
use crate::scheduler::{self, ScheduleEvent, SchedulerProxy, Timers};
use crate::scheduler::{ScheduleToken, TimerName, SCHEDULED_ARM};
use crate::SagaEffect;

pub(crate) struct InterpreterCtx<'a, S: Saga> {
    pub(crate) state: &'a mut S,
    pub(crate) saga_id: SagaId,
    pub(crate) sequence: &'a mut u64,
    pub(crate) store: Journal,
    pub(crate) retry_policy: RetryPolicy,
    pub(crate) process_ctx: &'a mut ProcessContext<SagaProcess<S>>,
    pub(crate) lifecycle: &'a mut Lifecycle,
    pub(crate) scheduler: Option<SchedulerProxy>,
    /// What the journal says is still pending, moved here as each schedule
    /// marker becomes durable so the saga's own `Deadline` view never reports a
    /// deadline whose fact did not land.
    pub(crate) pending_deadlines: PendingDeadlines,
    /// The timers whose `Scheduled` marker became durable while this effect was
    /// interpreted.
    ///
    /// Read by the deadline-firing path to tell whether the effect re-set the
    /// very deadline that fired: folding the journal replaces a schedule by
    /// name, so a same-name fact supersedes the one that fired on its own and
    /// the firing needs no settlement of its own.  Collected from what landed
    /// rather than from what the effect asked for, so a reset whose append never
    /// made it does not pass for one that did.
    pub(crate) landed_schedules: Vec<TimerName>,
    /// Where a deadline this effect registers must address its firing.
    pub(crate) delivery: &'a ScheduleDelivery<S>,
    /// How far each upstream source has been consumed, advanced here as each
    /// batch lands so the running incarnation answers the same question replay
    /// does.
    pub(crate) consumed: &'a mut ConsumedPositions,
    /// The upstream record this effect was decided from, when there is one.
    ///
    /// `None` for a deadline firing, which has no upstream input to consume —
    /// which is why nothing it decides records a position.
    pub(crate) input_position: Option<ConsumedPositionEvent>,
}

impl<S: Saga> InterpreterCtx<'_, S> {
    fn timers(&self) -> Option<Timers> {
        self.scheduler
            .as_ref()
            .map(|s| Timers::new(self.saga_id.clone(), s.clone()))
    }
}

pub(crate) enum InterpretOutcome {
    Continue,
    /// An append this effect required never landed, and the failure has already
    /// been reported.
    ///
    /// Interpretation carries on, which is why this is not `Stop`, but the
    /// input the effect was decided from is *not* consumed: a decision it owes
    /// is in no store.  The `Sequence` arm reads this to withhold the consumed
    /// position from the append that closes the input out, and the outermost
    /// caller reads it only to report the failure before still answering `Ok`
    /// — raising it would stop the saga through handler-failure supervision,
    /// which is exactly what must not happen here.  No cursor is held open for
    /// this: the live upstream cursor advances regardless, and recovery comes
    /// instead from what never became durable — nothing on the journal claims
    /// the record, so a later host's sweep of that source re-opens it.
    ///
    /// A deadline firing recovers the same way, through the row that settles
    /// *it*: no `fired` marker is written for this outcome, so the schedule fact
    /// stays unfired on the stream and a later activation re-arms the deadline
    /// from it — immediately, its due time being past — and the decision is made
    /// again.  Firing is at-least-once for that reason, and nothing is retried
    /// while the incarnation that failed is still alive.
    AppendFailed,
    Stop,
}

pub(crate) fn run_saga_effect<'a, S: Saga>(
    effect: SagaEffect<S::Event>,
    ictx: &'a mut InterpreterCtx<'_, S>,
) -> BoxFuture<'a, InterpretOutcome> {
    Box::pin(async move {
        // Normalize before interpreting: `SagaEffect`'s variants are public,
        // so a hand-built `Sequence(vec![Persist(..), Persist(..)])` can
        // reach this function without ever going through `combine`. Folding
        // the adjacent `Persist × Persist` junction here — the single choke
        // point every effect passes through — makes the atomic-batch
        // guarantee hold regardless of how the effect was constructed.
        match effect.normalize_for_interpretation() {
            // Routed through `persist_batch` with an empty batch rather than
            // answered with `Continue` directly: a successful `handle` that
            // decided nothing still owes a consumed-position append when the
            // input it was decided from has one pending, and `persist_batch`'s
            // empty-batch gate is what already knows whether that is the case
            // (see its comment below).
            SagaEffect::None => persist_batch(Vec::new(), Vec::new(), ictx).await,
            SagaEffect::Persist { events, schedules } => {
                persist_batch(events, schedules, ictx).await
            }
            SagaEffect::End => {
                // Persist the durable `Ended` lifecycle fact before
                // transitioning to the End state.  The revival guard relies on
                // this fact being present in the stream so a subsequent spawn
                // detects prior termination and refuses to revive the saga.  If
                // the append fails we do NOT stop: stopping without the fact
                // would leave the stream without the guard, allowing the saga
                // to be revived on the next spawn.  Remaining alive lets the
                // next incoming event retry the End decision.
                //
                // `input_position` is carried into the same append when `End`
                // is the effect closing out this input — see the `Sequence`
                // arm below — so the position is never durable before the
                // `Ended` fact it depends on is.
                if !lifecycle::append_ended(
                    &ictx.store,
                    &ictx.saga_id,
                    ictx.sequence,
                    ictx.input_position.as_ref(),
                    ictx.consumed,
                )
                .await
                {
                    return InterpretOutcome::AppendFailed;
                }

                // Set before stopping: `on_stop` reads this to tell a terminal
                // stop from a passivation (see `SagaProcess::on_stop`).
                *ictx.lifecycle = Lifecycle::Ended;
                if let Err(e) = ictx.process_ctx.stop_self().await {
                    tracing::warn!(error = %e, "saga End: stop_self signal failed");
                }
                InterpretOutcome::Stop
            }
            SagaEffect::Sequence(mut sub_effects) => {
                // `normalize_for_interpretation` only ever returns a
                // `Sequence` carrying two or more elements (see its own
                // `match flat.len()`), so there is always at least one
                // element left after the closing one is taken out.
                //
                // `End` short-circuits unconditionally — the public
                // contract on `SagaEffect::End` states "effects placed
                // after `End` inside a `Sequence` are not interpreted" —
                // so the append that actually lands last for this input is
                // not always the physically last element of the vector:
                // when `End` occurs before it, `End`'s own append is what
                // closes the input out, and every element physically after
                // it never runs at all.
                let closing_idx = sub_effects
                    .iter()
                    .position(|e| matches!(e, SagaEffect::End))
                    .unwrap_or(sub_effects.len() - 1);
                let closing = sub_effects.remove(closing_idx);
                sub_effects.truncate(closing_idx);
                // One `handle` call can reach the store as more than one
                // atomic append — a `CancelSchedule` or `End` is a hard
                // boundary the `Persist × Persist` fold does not cross — and
                // the position must never be durable before every decision
                // this input produced is durable too.  So only the append
                // that lands last for this Sequence may carry it; every
                // earlier one here runs with no input to consume attached.
                let position = ictx.input_position.take();
                let mut every_append_landed = true;
                for sub in sub_effects {
                    match run_saga_effect(sub, ictx).await {
                        InterpretOutcome::Stop => return InterpretOutcome::Stop,
                        // Reported, and interpretation goes on to the closing
                        // append — but this input now owes a decision no store
                        // holds, so it is not consumed.
                        InterpretOutcome::AppendFailed => every_append_landed = false,
                        InterpretOutcome::Continue => {}
                    }
                }
                // An input is consumed only once *every* append it required has
                // landed.  A missed append above therefore forfeits the position
                // for good: handing it to the closing append would make "this
                // input is dealt with" durable while a decision the same input
                // produced is not, and the redelivery that would have re-made
                // that decision is exactly what the position stops.
                if every_append_landed {
                    ictx.input_position = position;
                }
                // `End` is not just another closing append: interpreting it
                // writes the durable `Ended` fact that the revival guard
                // uses to refuse ever spawning this saga again (see the
                // `SagaEffect::End` arm above). Once that fact lands, no
                // later restart resubscribes from upstream, so the recovery
                // path every other `AppendFailed` relies on — a redelivery
                // that reaches `Saga::handle` again — is permanently closed.
                // A preceding append in this `Sequence` already failed and
                // was reported, so this input still owes a decision no store
                // holds; closing it out here would durably terminate the
                // saga around that gap.  Withhold the `End` append entirely
                // and report this closing step as failed too, leaving the
                // saga exactly as it was before this `handle` call — alive,
                // and reachable by redelivery on restart.
                if !every_append_landed && matches!(closing, SagaEffect::End) {
                    return InterpretOutcome::AppendFailed;
                }
                match run_saga_effect(closing, ictx).await {
                    // The parts that did land say nothing about the ones that
                    // did not: an enclosing `Sequence` has the same position to
                    // withhold.
                    InterpretOutcome::Continue if !every_append_landed => {
                        InterpretOutcome::AppendFailed
                    }
                    outcome => outcome,
                }
            }
            SagaEffect::CancelSchedule(name) => cancel_schedule(name, ictx).await,
        }
    })
}

/// Interpret `CancelSchedule(name)`: append a `ScheduleEvent::Cancelled` marker
/// — plus the consumed position, when this is the effect closing out the
/// input — in one atomic append (sequence advances only on success), and
/// cancel the pending timer with the resident scheduler.
async fn cancel_schedule<S: Saga>(
    name: TimerName,
    ictx: &mut InterpreterCtx<'_, S>,
) -> InterpretOutcome {
    let token = ScheduleToken::new(ictx.saga_id.clone(), name.clone());
    let event = ScheduleEvent::Cancelled {
        token,
        cancelled_at_unix_millis: jiff::Timestamp::now().as_millisecond(),
    };
    let appended = scheduler::append_schedule_marker(
        &ictx.store,
        &ictx.saga_id,
        ictx.sequence,
        event,
        jiff::Timestamp::now(),
        ictx.input_position.as_ref(),
        ictx.consumed,
    )
    .await;
    if !appended {
        // Leave the timer registered: without the durable Cancelled marker a
        // later replay would re-register it, so cancelling the live timer now
        // would diverge from the persisted state.  The next effect can retry.
        return InterpretOutcome::AppendFailed;
    }
    // The marker is durable, so the deadline is no longer pending by the only
    // account that matters — the journal's.
    ictx.pending_deadlines.forget(&name);
    if let Some(timers) = ictx.timers() {
        timers.cancel(name).await;
    }
    InterpretOutcome::Continue
}

async fn persist_batch<S: Saga>(
    events: Vec<S::Event>,
    schedules: Vec<ScheduleSpec>,
    ictx: &mut InterpreterCtx<'_, S>,
) -> InterpretOutcome {
    // A batch with nothing to persist still owes an append when the input it
    // was decided from has a position that is not yet durable: otherwise a
    // successful `handle` that writes nothing — `SagaEffect::None`, or a
    // `Persist` folded down to nothing — would leave no record of having
    // consumed its input, and redelivery would reach `Saga::handle` again for
    // a decision already made. `position_pending` mirrors the condition
    // `append_consumed_position` itself gates on below, so the two agree on
    // when there is nothing left to write.
    let position_pending = ictx.input_position.as_ref().is_some_and(|position| {
        !ictx.consumed.covers(
            position.source.as_str(),
            &position.source_event_type,
            position.sequence,
        )
    });
    if events.is_empty() && schedules.is_empty() && !position_pending {
        return InterpretOutcome::Continue;
    }

    let now = jiff::Timestamp::now();
    let mut next_seq = *ictx.sequence;

    let mut appending: Vec<AppendingEvent> = Vec::with_capacity(events.len() + schedules.len() + 1);
    if !append_user_events::<S>(&mut appending, &events, &mut next_seq, now) {
        return InterpretOutcome::AppendFailed;
    }
    let registrations = append_schedule_specs(
        &mut appending,
        &ictx.saga_id,
        &schedules,
        &mut next_seq,
        now,
    );
    // The position closes the batch: placed anywhere earlier it would shift
    // every row after it, and the order within the batch is what an operator
    // reads the decision and the input it settles off.
    let recorded_position = append_consumed_position(
        &mut appending,
        ictx.input_position.as_ref(),
        ictx.consumed,
        &mut next_seq,
        now,
    );

    // Staged retry.  Sequence stays at its pre-batch value throughout so no
    // sequence numbers are skipped on failure.
    let mut attempt = 0;
    let succeeded = loop {
        attempt += 1;
        if attempt > 1 {
            tokio::time::sleep(ictx.retry_policy.backoff_before(attempt)).await;
        }
        match ictx
            .store
            .append(ictx.saga_id.borrow(), appending.clone())
            .await
        {
            Ok(_) => break true,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    attempt,
                    max_attempts = ictx.retry_policy.max_attempts,
                    "saga atomic persist batch failed; will retry if attempts remain"
                );
                if attempt >= ictx.retry_policy.max_attempts {
                    break false;
                }
            }
        }
    };
    if !succeeded {
        tracing::error!(
            saga_id = ictx.saga_id.as_str(),
            max_attempts = ictx.retry_policy.max_attempts,
            "saga persist batch exhausted staged retries; the input it was decided \
             from stays unconsumed so a redelivery can decide it again"
        );
        return InterpretOutcome::AppendFailed;
    }

    *ictx.sequence = next_seq;

    for event in events {
        ictx.state.apply(event);
    }

    // The input is consumed exactly when the batch carrying its position is
    // durable, so the in-memory position moves here and nowhere else.
    finalize_consumed_position(ictx.consumed, ictx.saga_id.as_str(), recorded_position);

    // The schedule markers of this batch are durable now, so what they record
    // can be reported — and only now may the pending view hold them, or the
    // firing path read them as a deadline this effect actually re-set: a batch
    // that never landed must leave both exactly as they were. The timers
    // themselves are registered further below, after every marker has landed.
    for reg in &registrations {
        ictx.pending_deadlines.record(
            reg.name.clone(),
            deadline::due_at(reg.scheduled_at_unix_millis, reg.after),
        );
        ictx.landed_schedules.push(reg.name.clone());
        report_system_event!(
            ScheduleEvent::EVENT_TYPE,
            SCHEDULED_ARM,
            ictx.saga_id.as_str(),
            timer = reg.name.as_str(),
        );
    }

    // Register timers only after the `Scheduled` markers are durably persisted,
    // so a firing can never precede its own durable record.
    if let Some(timers) = ictx.timers() {
        for reg in registrations {
            let dispatch = ictx.delivery.dispatch(
                &ictx.saga_id,
                reg.name.clone(),
                reg.message_type,
                reg.message_codec,
                reg.payload,
            );
            timers.start_single(reg.name, reg.after, dispatch).await;
        }
    } else if !schedules.is_empty() {
        // Unreachable: a `ScheduleSpec` can only come from a `Deadline<M>`,
        // `SagaComponents::deadline::<M>()` only exists where `S::Subscribe:
        // SagaSubscribe<Deadline = HasDeadline<M>>`, and `resolve_for` resolves
        // the resident timer core from that exact same associated type (see
        // `scheduler::resolve_for`). A saga whose alphabet can produce this
        // batch's schedules therefore always has `ictx.scheduler` populated —
        // there is no longer a separately-settable fact the two could disagree
        // about, including for a hand-written `SagaSubscribe`, since `Deadline`
        // is sealed to `NoDeadline` and `HasDeadline<M>`.
        unreachable!(
            "saga {} persisted schedule markers with no resident timer core resolved; \
             SagaSubscribe::Deadline and the spawn boundary's resolution disagreed",
            ictx.saga_id.as_str()
        );
    }

    InterpretOutcome::Continue
}

/// Add the row each of the saga's own events is written down as to the batch.
///
/// Answers `false` when a codec refused one of them, having reported it. The
/// batch is atomic, so a refusal anywhere in it means none of it is written:
/// the caller abandons the batch before reaching the store, leaving the saga's
/// sequence where it was.
fn append_user_events<S: Saga>(
    appending: &mut Vec<AppendingEvent>,
    events: &[S::Event],
    next_seq: &mut u64,
    now: jiff::Timestamp,
) -> bool {
    for event in events {
        match event.appending(*next_seq + 1, now) {
            Ok(row) => {
                *next_seq += 1;
                appending.push(row);
            }
            Err(e) => {
                tracing::warn!(error = %e, "saga event encode failed; skipping persist batch");
                return false;
            }
        }
    }
    true
}

/// A timer to register with the resident scheduler after the atomic batch has
/// been persisted.
struct PendingRegistration {
    name: TimerName,
    after: std::time::Duration,
    /// The identity written into the fact beside `payload` — see
    /// `ScheduleEvent::Scheduled` — carried through so the dispatch registered
    /// with the resident scheduler can hand it to the firing it later reads.
    message_type: String,
    message_codec: String,
    payload: bytes::Bytes,
    /// The wall-clock anchor written into the fact, so the pending view derives
    /// the due instant from the same two numbers a replay would.
    scheduled_at_unix_millis: i64,
}

/// Append one `ScheduleEvent::Scheduled` marker per requested timer into the
/// atomic batch, returning the data needed to register each timer once the
/// append succeeds.
///
/// `scheduled_at` is captured as a wall-clock anchor so replay can recompute
/// the remaining delay (`remaining = scheduled_at + after - now`).
fn append_schedule_specs(
    appending: &mut Vec<AppendingEvent>,
    saga_id: &SagaId,
    schedules: &[ScheduleSpec],
    next_seq: &mut u64,
    now: jiff::Timestamp,
) -> Vec<PendingRegistration> {
    let scheduled_at_unix_millis = now.as_millisecond();
    let mut registrations = Vec::with_capacity(schedules.len());
    for spec in schedules {
        *next_seq += 1;
        let token = ScheduleToken::new(saga_id.clone(), spec.name.clone());
        let event = ScheduleEvent::Scheduled {
            token,
            after: spec.after,
            message_type: spec.message_type.to_owned(),
            message_codec: spec.message_codec.to_owned(),
            payload: spec.payload.clone(),
            scheduled_at_unix_millis,
        };
        appending.push(event.appending(*next_seq, now));
        registrations.push(PendingRegistration {
            name: spec.name.clone(),
            after: spec.after,
            message_type: spec.message_type.to_owned(),
            message_codec: spec.message_codec.to_owned(),
            payload: spec.payload.clone(),
            scheduled_at_unix_millis,
        });
    }
    registrations
}
