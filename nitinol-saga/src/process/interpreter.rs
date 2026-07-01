//! Outbox-backed interpreter for [`SagaEffect`].

use std::borrow::Borrow;
use std::collections::HashMap;
use std::sync::Arc;

use bytes::Bytes;
use futures_core::future::BoxFuture;
use nitinol_eventsource::codec::ErasedCodec;
use nitinol_eventsource::Event;
use nitinol_persistence::store::EventStore;
use nitinol_persistence::{AppendingEvent, EventType};
use nitinol_runtime::process::ProcessContext;

use crate::effect::{Schedule, TellIntent};
use crate::id::SagaId;
use crate::outbox::{OutboxAppender, RetryPolicy};
use crate::process::outbox_executor::spawn_outbox_executor;
use crate::process::saga_process::{in_flight_count, Lifecycle, SagaProcess, TellState};
use crate::saga::Saga;
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
}

pub(crate) enum InterpretOutcome {
    Continue,
    Stop,
}

pub(crate) fn run_saga_effect<'a, S: Saga>(
    effect: SagaEffect<S::Event>,
    ictx: &'a mut InterpreterCtx<'_, S>,
) -> BoxFuture<'a, InterpretOutcome> {
    Box::pin(async move {
        match effect {
            SagaEffect::None => InterpretOutcome::Continue,
            SagaEffect::Persist {
                events,
                tells,
                schedules,
            } => persist_batch(events, tells, schedules, ictx).await,
            SagaEffect::End => {
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
                    if matches!(run_saga_effect(sub, ictx).await, InterpretOutcome::Stop) {
                        return InterpretOutcome::Stop;
                    }
                }
                InterpretOutcome::Continue
            }
        }
    })
}

async fn persist_batch<S: Saga>(
    events: Vec<S::Event>,
    tells: Vec<TellIntent>,
    schedules: Vec<Schedule>,
    ictx: &mut InterpreterCtx<'_, S>,
) -> InterpretOutcome {
    if events.is_empty() && tells.is_empty() && schedules.is_empty() {
        return InterpretOutcome::Continue;
    }

    let encoded_events = match encode_events::<S>(&events, ictx.codec.as_ref()) {
        Some(payloads) => payloads,
        None => return InterpretOutcome::Continue,
    };

    let now = jiff::Timestamp::now();
    let mut next_seq = *ictx.sequence;

    let mut appending: Vec<AppendingEvent> =
        Vec::with_capacity(encoded_events.len() + tells.len() + schedules.len());
    append_user_events(&mut appending, encoded_events, &mut next_seq, now);
    let tell_ids = append_tell_requested(&mut appending, &tells, &mut next_seq, now);
    append_scheduled(&mut appending, &schedules, &mut next_seq, now);

    if let Err(e) = ictx.store.append(ictx.saga_id.borrow(), appending).await {
        // Sequence stays at its pre-batch value so the next attempt does not
        // skip sequence numbers.
        tracing::warn!(error = %e, "saga atomic persist batch failed; skipping apply");
        return InterpretOutcome::Continue;
    }

    *ictx.sequence = next_seq;

    for event in events {
        ictx.state.apply(event);
    }

    for (intent, tell_id) in tells.into_iter().zip(tell_ids) {
        ictx.tell_states
            .insert(tell_id, TellState::Pending(intent.clone()));
        let policy = ictx.retry_policy.clone();
        spawn_outbox_executor(ictx.process_ctx, intent, tell_id, policy).await;
    }

    InterpretOutcome::Continue
}

fn encode_events<S: Saga>(
    events: &[S::Event],
    codec: &dyn ErasedCodec<S::Event>,
) -> Option<Vec<(EventType, Bytes)>> {
    let mut encoded = Vec::with_capacity(events.len());
    for event in events {
        match codec.encode(event) {
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
            now,
        ));
    }
    tell_ids
}

fn append_scheduled(
    appending: &mut Vec<AppendingEvent>,
    schedules: &[Schedule],
    next_seq: &mut u64,
    now: jiff::Timestamp,
) {
    for schedule in schedules {
        *next_seq += 1;
        appending.push(OutboxAppender::build_scheduled(*next_seq, schedule.at, now));
    }
}
