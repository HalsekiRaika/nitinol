//! Outbox-backed interpreter for [`SagaEffect`].

use std::borrow::Borrow;
use std::collections::HashMap;
use std::sync::Arc;

use futures_core::future::BoxFuture;
use nitinol_eventsource::{EventAppending, SystemEvent};
use nitinol_persistence::store::EventStore;
use nitinol_persistence::AppendingEvent;
use nitinol_runtime::process::ProcessContext;

use crate::consumed::{
    append_consumed_position, finalize_consumed_position, ConsumedPositionEvent, ConsumedPositions,
};
use crate::dead_letter::{self, EnqueueOutcome, EnqueuePolicy, SagaFailure, SourceContext};
use crate::effect::{ScheduleSpec, TellIntent};
use crate::id::SagaId;
use crate::observability::report_system_event;
use crate::outbox::{OutboxAppender, OutboxEvent, RetryPolicy, TELL_REQUESTED_ARM};
use crate::process::instance::ScheduleDelivery;
use crate::process::outbox_executor;
use crate::process::saga_process::{self, Lifecycle, SagaProcess, TellState};
use crate::saga::Saga;
use crate::scheduler::{self, ScheduleEvent, SchedulerProxy, Timers};
use crate::scheduler::{ScheduleToken, TimerName, SCHEDULED_ARM};
use crate::SagaEffect;

pub(crate) struct InterpreterCtx<'a, S: Saga> {
    pub(crate) state: &'a mut S,
    pub(crate) saga_id: SagaId,
    pub(crate) sequence: &'a mut u64,
    pub(crate) store: Arc<dyn EventStore>,
    pub(crate) retry_policy: RetryPolicy,
    pub(crate) process_ctx: &'a mut ProcessContext<SagaProcess<S>>,
    pub(crate) tell_states: &'a mut HashMap<u64, TellState>,
    pub(crate) lifecycle: &'a mut Lifecycle,
    pub(crate) scheduler: Option<SchedulerProxy>,
    /// Where a deadline this effect registers must address its firing.
    pub(crate) delivery: &'a ScheduleDelivery<S>,
    pub(crate) enqueue_policy: Arc<dyn EnqueuePolicy>,
    /// How far each upstream source has been consumed, advanced here as each
    /// batch lands so the running incarnation answers the same question replay
    /// does.
    pub(crate) consumed: &'a mut ConsumedPositions,
    /// The upstream record this effect was decided from, when there is one.
    ///
    /// `None` for the entry points that have no upstream input to consume — a
    /// timer firing and the tell-failure hook — which is why nothing they
    /// persist records a position.
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
    /// been reported — as a dead letter, or as a warning where the batch was
    /// abandoned before it reached the store.
    ///
    /// Interpretation carries on, which is why this is not `Stop`, but the
    /// input the effect was decided from is *not* consumed: a decision it owes
    /// is in no store.  The `Sequence` arm reads this to withhold the consumed
    /// position from the append that closes the input out; the outermost
    /// callers, which only ask whether the dead letter itself landed, treat it
    /// the same as `Continue`.
    AppendFailed,
    Stop,
    /// A dead-letter append failed inside the interpreter for an upstream-
    /// delivered message.  The caller (`Receive<UpstreamMessage>::recv`) must
    /// propagate this as `Err(SagaUpstreamHandlerError)` so the `DirectPoller`
    /// (via `ask()`) does not advance the upstream cursor.
    DlqFailed,
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
            SagaEffect::None => persist_batch(Vec::new(), Vec::new(), Vec::new(), ictx).await,
            SagaEffect::Persist {
                events,
                tells,
                schedules,
            } => persist_batch(events, tells, schedules, ictx).await,
            SagaEffect::End => {
                // Persist the durable `Ended` marker before transitioning to the
                // End state.  The revival guard relies on this marker being
                // present in the stream so a subsequent spawn detects prior termination
                // and refuse to revive the saga.  If the append fails we do NOT
                // stop: stopping without the marker would leave the stream without
                // the guard, allowing the saga to be revived on the next spawn.
                // Remaining alive lets the next incoming event retry the End
                // decision.
                //
                // `input_position` is carried into the same append when `End`
                // is the effect closing out this input — see the `Sequence`
                // arm below — so the position is never durable before the
                // `Ended` marker it depends on is.
                if !OutboxAppender::append_ended(
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

                // Set unconditionally, before either branch below: `on_stop`
                // reads this to tell a terminal stop from a passivation (see
                // `SagaProcess::on_stop`), and that distinction must hold
                // whether or not a tell was still in flight when `End` landed.
                *ictx.lifecycle = Lifecycle::Draining;
                if saga_process::in_flight_count(ictx.tell_states) == 0 {
                    if let Err(e) = ictx.process_ctx.stop_self().await {
                        tracing::warn!(error = %e, "saga End: stop_self signal failed");
                    }
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
                        InterpretOutcome::DlqFailed => return InterpretOutcome::DlqFailed,
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
                // writes the durable `Ended` marker that the revival guard
                // uses to refuse ever spawning this saga again (see the
                // `SagaEffect::End` arm above). Once that marker lands, no
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
    if let Some(timers) = ictx.timers() {
        timers.cancel(name).await;
    }
    InterpretOutcome::Continue
}

async fn persist_batch<S: Saga>(
    events: Vec<S::Event>,
    tells: Vec<TellIntent>,
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
        !ictx
            .consumed
            .covers(position.source.as_str(), position.sequence)
    });
    if events.is_empty() && tells.is_empty() && schedules.is_empty() && !position_pending {
        return InterpretOutcome::Continue;
    }

    let now = jiff::Timestamp::now();
    let mut next_seq = *ictx.sequence;

    let mut appending: Vec<AppendingEvent> =
        Vec::with_capacity(events.len() + tells.len() + schedules.len() + 1);
    if !append_user_events::<S>(&mut appending, &events, &mut next_seq, now) {
        return InterpretOutcome::AppendFailed;
    }
    let tell_ids = append_tell_requested(&mut appending, &tells, &mut next_seq, now);
    let registrations = append_schedule_specs(
        &mut appending,
        &ictx.saga_id,
        &schedules,
        &mut next_seq,
        now,
    );
    // The position closes the batch.  Placed anywhere earlier it would shift
    // every row after it, and a `tell_id` is the stream sequence of its own
    // `TellRequested` row — the identity the outbox re-dispatch path resolves a
    // pending tell by.
    let recorded_position = append_consumed_position(
        &mut appending,
        ictx.input_position.as_ref(),
        ictx.consumed,
        &mut next_seq,
        now,
    );

    // Staged retry before DLQ.  Sequence stays at its pre-batch value
    // throughout so no sequence numbers are skipped on failure.
    let mut last_error = String::new();
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
                last_error = e.to_string();
                if attempt >= ictx.retry_policy.max_attempts {
                    break false;
                }
            }
        }
    };
    if !succeeded {
        tracing::warn!(
            max_attempts = ictx.retry_policy.max_attempts,
            "saga persist batch exhausted staged retries; enqueuing PersistFailed dead letter"
        );
        // `input_position: None` — the batch that would have carried the
        // position is exactly what never landed, so this dead letter must not
        // mark the input it reports on as consumed.
        let outcome = dead_letter::enqueue_dead_letter(
            &ictx.store,
            &ictx.saga_id,
            ictx.sequence,
            ictx.enqueue_policy.as_ref(),
            SagaFailure::PersistFailed { error: last_error },
            SourceContext::without_upstream(),
            None,
            ictx.consumed,
        )
        .await;
        // When the DLQ append also fails, signal DlqFailed so the
        // caller can propagate Err through recv(), preventing the DirectPoller
        // (via ask()) from advancing the upstream cursor.  For non-upstream
        // callers (e.g. FireScheduled), the caller must call stop_self() on
        // receiving DlqFailed.
        if matches!(outcome, EnqueueOutcome::AppendFailed) {
            tracing::error!(
                saga_id = ictx.saga_id.as_str(),
                "saga DLQ append failed for PersistFailed; signalling DlqFailed"
            );
            return InterpretOutcome::DlqFailed;
        }
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
    // can be reported. The timers themselves are registered further below,
    // after every marker has landed.
    for reg in &registrations {
        report_system_event!(
            ScheduleEvent::EVENT_TYPE,
            SCHEDULED_ARM,
            ictx.saga_id.as_str(),
            timer = reg.name.as_str(),
        );
    }

    for (intent, tell_id) in tells.into_iter().zip(tell_ids) {
        report_system_event!(
            OutboxEvent::EVENT_TYPE,
            TELL_REQUESTED_ARM,
            ictx.saga_id.as_str(),
            tell_id = tell_id,
            target = intent.target_id.as_str(),
        );
        ictx.tell_states
            .insert(tell_id, TellState::Pending(intent.clone()));
        let policy = ictx.retry_policy.clone();
        outbox_executor::spawn_outbox_executor(ictx.process_ctx, intent, tell_id, policy).await;
    }

    // Register timers only after the `Scheduled` markers are durably persisted,
    // so a firing can never precede its own durable record.
    if let Some(timers) = ictx.timers() {
        for reg in registrations {
            let dispatch = ictx.delivery.dispatch(
                &ictx.saga_id,
                ictx.process_ctx,
                reg.name.clone(),
                reg.payload,
            );
            timers.start_single(reg.name, reg.after, dispatch).await;
        }
    } else if !schedules.is_empty() {
        tracing::warn!(
            saga_id = ictx.saga_id.as_str(),
            "saga persisted schedule markers but no scheduler was injected \
             (SagaProps::with_scheduler); timers will not fire until replayed \
             by a scheduler-equipped incarnation"
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

fn append_tell_requested(
    appending: &mut Vec<AppendingEvent>,
    tells: &[TellIntent],
    next_seq: &mut u64,
    now: jiff::Timestamp,
) -> Vec<u64> {
    let mut tell_ids = Vec::with_capacity(tells.len());
    for intent in tells {
        *next_seq += 1;
        let tell_id = *next_seq;
        tell_ids.push(tell_id);
        appending.push(OutboxAppender::build_tell_requested(
            *next_seq,
            tell_id,
            intent.crash_restart_payload.as_deref(),
            intent.target_id.as_str(),
            now,
        ));
    }
    tell_ids
}

/// A timer to register with the resident scheduler after the atomic batch has
/// been persisted.
struct PendingRegistration {
    name: TimerName,
    after: std::time::Duration,
    payload: bytes::Bytes,
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
            payload: spec.payload.clone(),
            scheduled_at_unix_millis,
        };
        appending.push(event.appending(*next_seq, now));
        registrations.push(PendingRegistration {
            name: spec.name.clone(),
            after: spec.after,
            payload: spec.payload.clone(),
        });
    }
    registrations
}
