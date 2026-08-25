//! Outbox-backed interpreter for [`SagaEffect`].

use std::borrow::Borrow;
use std::collections::HashMap;
use std::sync::Arc;

use bytes::Bytes;
use futures_core::future::BoxFuture;
use nitinol_eventsource::codec::ErasedCodec;
use nitinol_persistence::store::EventStore;
use nitinol_persistence::{AppendingEvent, EventType};
use nitinol_runtime::process::ProcessContext;

use crate::dead_letter::{
    enqueue_dead_letter, EnqueueOutcome, EnqueuePolicy, SagaFailure, SourceContext,
};
use crate::effect::{ScheduleSpec, TellIntent};
use crate::id::SagaId;
use crate::outbox::{OutboxAppender, RetryPolicy};
use crate::persisted::SagaPersisted;
use crate::process::outbox_executor::spawn_outbox_executor;
use crate::process::saga_process::{
    build_fire_dispatch, in_flight_count, Lifecycle, SagaProcess, TellState,
};
use crate::saga::Saga;
use crate::scheduler::{append_schedule_marker, ScheduleEvent, SchedulerProxy, Timers};
use crate::scheduler::{ScheduleToken, TimerName};
use crate::SagaEffect;

pub(crate) struct InterpreterCtx<'a, S: Saga> {
    pub(crate) state: &'a mut S,
    pub(crate) saga_id: SagaId,
    pub(crate) sequence: &'a mut u64,
    pub(crate) store: Arc<dyn EventStore>,
    pub(crate) codec: Arc<dyn ErasedCodec<S::Event>>,
    pub(crate) retry_policy: RetryPolicy,
    pub(crate) process_ctx: &'a mut ProcessContext<SagaProcess<S>>,
    pub(crate) tell_states: &'a mut HashMap<u64, TellState>,
    pub(crate) lifecycle: &'a mut Lifecycle,
    pub(crate) scheduler: Option<SchedulerProxy>,
    pub(crate) enqueue_policy: Arc<dyn EnqueuePolicy>,
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
            SagaEffect::None => InterpretOutcome::Continue,
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
                let candidate = *ictx.sequence + 1;
                if !OutboxAppender::append_ended(&ictx.store, &ictx.saga_id, candidate).await {
                    return InterpretOutcome::Continue;
                }
                *ictx.sequence = candidate;

                if in_flight_count(ictx.tell_states) == 0 {
                    if let Err(e) = ictx.process_ctx.stop_self().await {
                        tracing::warn!(error = %e, "saga End: stop_self signal failed");
                    }
                } else {
                    *ictx.lifecycle = Lifecycle::Draining;
                }
                InterpretOutcome::Stop
            }
            SagaEffect::Sequence(sub_effects) => {
                for sub in sub_effects {
                    match run_saga_effect(sub, ictx).await {
                        InterpretOutcome::Stop => return InterpretOutcome::Stop,
                        InterpretOutcome::DlqFailed => return InterpretOutcome::DlqFailed,
                        InterpretOutcome::Continue => {}
                    }
                }
                InterpretOutcome::Continue
            }
            SagaEffect::CancelSchedule(name) => {
                cancel_schedule(name, ictx).await;
                InterpretOutcome::Continue
            }
        }
    })
}

/// Interpret `CancelSchedule(name)`: append a `ScheduleEvent::Cancelled` marker
/// (single append, sequence advances only on success) and cancel the pending
/// timer with the resident scheduler.
async fn cancel_schedule<S: Saga>(name: TimerName, ictx: &mut InterpreterCtx<'_, S>) {
    let token = ScheduleToken {
        saga_id: ictx.saga_id.clone(),
        name: name.clone(),
    };
    let event = ScheduleEvent::Cancelled {
        token,
        cancelled_at_unix_millis: jiff::Timestamp::now().as_millisecond(),
    };
    let appended = append_schedule_marker(
        &ictx.store,
        &ictx.saga_id,
        ictx.sequence,
        event,
        jiff::Timestamp::now(),
    )
    .await;
    if !appended {
        // Leave the timer registered: without the durable Cancelled marker a
        // later replay would re-register it, so cancelling the live timer now
        // would diverge from the persisted state.  The next effect can retry.
        return;
    }
    if let Some(timers) = ictx.timers() {
        timers.cancel(name).await;
    }
}

async fn persist_batch<S: Saga>(
    events: Vec<S::Event>,
    tells: Vec<TellIntent>,
    schedules: Vec<ScheduleSpec>,
    ictx: &mut InterpreterCtx<'_, S>,
) -> InterpretOutcome {
    if events.is_empty() && tells.is_empty() && schedules.is_empty() {
        return InterpretOutcome::Continue;
    }

    let persisted: Vec<SagaPersisted<S::Event>> =
        events.into_iter().map(SagaPersisted::Domain).collect();

    let encoded_events = match encode_events::<S>(&persisted, ictx.codec.as_ref()) {
        Some(payloads) => payloads,
        None => return InterpretOutcome::Continue,
    };

    let now = jiff::Timestamp::now();
    let mut next_seq = *ictx.sequence;

    let mut appending: Vec<AppendingEvent> =
        Vec::with_capacity(encoded_events.len() + tells.len() + schedules.len());
    append_user_events(&mut appending, encoded_events, &mut next_seq, now);
    let tell_ids = append_tell_requested(&mut appending, &tells, &mut next_seq, now);
    let registrations = append_schedule_specs(
        &mut appending,
        &ictx.saga_id,
        &schedules,
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
        let outcome = enqueue_dead_letter(
            &ictx.store,
            &ictx.saga_id,
            ictx.sequence,
            ictx.enqueue_policy.as_ref(),
            SagaFailure::PersistFailed { error: last_error },
            SourceContext::without_upstream(),
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
        return InterpretOutcome::Continue;
    }

    *ictx.sequence = next_seq;

    for persisted_event in persisted {
        match persisted_event {
            SagaPersisted::Domain(event) => ictx.state.apply(event),
            SagaPersisted::Outbox(_)
            | SagaPersisted::Schedule(_)
            | SagaPersisted::DeadLetter(_)
            | SagaPersisted::DeadLetterDisposition(_) => {
                unreachable!("persist_batch enqueues only Domain events into the envelope")
            }
        }
    }

    for (intent, tell_id) in tells.into_iter().zip(tell_ids) {
        ictx.tell_states
            .insert(tell_id, TellState::Pending(intent.clone()));
        let policy = ictx.retry_policy.clone();
        spawn_outbox_executor(ictx.process_ctx, intent, tell_id, policy).await;
    }

    // Register timers only after the `Scheduled` markers are durably persisted,
    // so a firing can never precede its own durable record.
    if let Some(timers) = ictx.timers() {
        for reg in registrations {
            let dispatch = build_fire_dispatch::<S>(
                ictx.process_ctx.self_proxy().clone(),
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

fn encode_events<S: Saga>(
    events: &[SagaPersisted<S::Event>],
    codec: &dyn ErasedCodec<S::Event>,
) -> Option<Vec<(EventType, Bytes)>> {
    let mut encoded = Vec::with_capacity(events.len());
    for event in events {
        match event.encode(codec) {
            Ok(payload) => encoded.push((event.variant(), payload)),
            Err(e) => {
                tracing::warn!(error = %e, "saga event encode failed; skipping persist batch");
                return None;
            }
        }
    }
    Some(encoded)
}

fn append_user_events(
    appending: &mut Vec<AppendingEvent>,
    encoded: Vec<(EventType, Bytes)>,
    next_seq: &mut u64,
    now: jiff::Timestamp,
) {
    for (event_type, payload) in encoded {
        *next_seq += 1;
        appending.push(AppendingEvent {
            sequence: *next_seq,
            event_type,
            payload,
            occurred_at: now,
        });
    }
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
        let token = ScheduleToken {
            saga_id: saga_id.clone(),
            name: spec.name.clone(),
        };
        let event = ScheduleEvent::Scheduled {
            token,
            after: spec.after,
            payload: spec.payload.clone(),
            scheduled_at_unix_millis,
        };
        appending.push(nitinol_eventsource::appending_system_event(
            *next_seq, &event, now,
        ));
        registrations.push(PendingRegistration {
            name: spec.name.clone(),
            after: spec.after,
            payload: spec.payload.clone(),
        });
    }
    registrations
}
