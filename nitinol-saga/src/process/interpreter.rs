//! Outbox-backed interpreter for [`SagaEffect`].
//!
//! Walks the effect ADT depth-first.  `Persist` branches go through
//! [`persist_batch`], which builds the entire atomic batch
//! (user events + `TellRequested` markers + `Scheduled` markers) and submits
//! it as one `EventStore::append` call.  `End` triggers `stop_self()` on the
//! process and short-circuits any enclosing `Sequence`.

use std::borrow::Borrow;
use std::sync::Arc;

use bytes::Bytes;
use futures_core::future::BoxFuture;
use nitinol_eventsource::codec::ErasedCodec;
use nitinol_eventsource::Event;
use nitinol_persistence::store::EventStore;
use nitinol_persistence::AppendingEvent;
use nitinol_runtime::process::ProcessContext;
use tokio::sync::Mutex;

use crate::effect::{Schedule, TellIntent};
use crate::id::SagaId;
use crate::outbox::{OutboxAppender, RetryPolicy};
use crate::process::outbox_executor::spawn_outbox_executor;
use crate::process::pending_intents::PendingIntents;
use crate::saga::Saga;
use crate::SagaEffect;

pub(crate) struct InterpreterCtx<'a, S: Saga> {
    pub(crate) state: &'a mut S,
    pub(crate) saga_id: SagaId,
    pub(crate) sequence: Arc<Mutex<u64>>,
    pub(crate) store: Arc<dyn EventStore>,
    pub(crate) codec: Arc<dyn ErasedCodec<S::Event>>,
    pub(crate) retry_policy: RetryPolicy,
    pub(crate) process_ctx: &'a ProcessContext,
    pub(crate) pending_intents: PendingIntents,
    /// Shared accumulator that the outbox executor pushes `tell_id` into when
    /// it appends a `TellFailed` terminal marker.  The saga process drains this
    /// before each `Saga::handle` invocation so the handler can inspect
    /// `SagaContext::failed_tell_ids()`.
    pub(crate) failed_tell_ids: Arc<Mutex<Vec<u64>>>,
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
                if let Err(e) = ictx.process_ctx.stop_self().await {
                    tracing::warn!(error = %e, "saga End: stop_self signal failed");
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
    let mut seq_guard = ictx.sequence.lock().await;
    let mut next_seq = *seq_guard;

    let mut appending: Vec<AppendingEvent> =
        Vec::with_capacity(encoded_events.len() + tells.len() + schedules.len());
    append_user_events::<S>(&mut appending, encoded_events, &mut next_seq, now);
    let tell_ids = append_tell_requested(&mut appending, &tells, &mut next_seq, now);
    append_scheduled(&mut appending, &schedules, &mut next_seq, now);

    if let Err(e) = ictx
        .store
        .append(ictx.saga_id.borrow(), appending)
        .await
    {
        // Sequence stays at its pre-batch value so the next attempt does not
        // skip sequence numbers.
        drop(seq_guard);
        tracing::warn!(error = %e, "saga atomic persist batch failed; skipping apply");
        return InterpretOutcome::Continue;
    }

    *seq_guard = next_seq;
    drop(seq_guard);

    for event in events {
        ictx.state.apply(event);
    }

    for (intent, tell_id) in tells.into_iter().zip(tell_ids) {
        ictx.pending_intents.register(tell_id, intent.clone()).await;
        spawn_outbox_executor(
            intent,
            tell_id,
            ictx.retry_policy.clone(),
            ictx.saga_id.clone(),
            Arc::clone(&ictx.store),
            Arc::clone(&ictx.sequence),
            ictx.pending_intents.clone(),
            Arc::clone(&ictx.failed_tell_ids),
        );
    }

    InterpretOutcome::Continue
}

fn encode_events<S: Saga>(
    events: &[S::Event],
    codec: &dyn ErasedCodec<S::Event>,
) -> Option<Vec<Bytes>> {
    let mut encoded = Vec::with_capacity(events.len());
    for event in events {
        match codec.encode(event) {
            Ok(payload) => encoded.push(payload),
            Err(e) => {
                tracing::warn!(error = %e, "saga event encode failed; skipping persist batch");
                return None;
            }
        }
    }
    Some(encoded)
}

fn append_user_events<S: Saga>(
    appending: &mut Vec<AppendingEvent>,
    encoded: Vec<Bytes>,
    next_seq: &mut u64,
    now: jiff::Timestamp,
) {
    for payload in encoded {
        *next_seq += 1;
        appending.push(AppendingEvent {
            sequence: *next_seq,
            event_type: <S::Event as Event>::EVENT_TYPE,
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
        appending.push(OutboxAppender::build_scheduled(
            *next_seq,
            schedule.at,
            now,
        ));
    }
}
