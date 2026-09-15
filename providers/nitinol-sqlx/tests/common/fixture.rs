//! The events the provider's contract tests append, and what a store served
//! back.
//!
//! The instant every event carries is fixed rather than taken from the clock,
//! so a round trip is compared against the value the caller supplied and two
//! runs of the same test compare the same thing.
//!
//! Each test binary compiles this module in full but uses only the subset it
//! needs, so per-binary dead code is expected.

#![allow(dead_code)]

use std::sync::Arc;

use bytes::Bytes;
use futures_util::TryStreamExt;
use jiff::Timestamp;
use nitinol_persistence::error::AppendError;
use nitinol_persistence::store::EventStore;
use nitinol_persistence::{
    AppendOutcome, AppendingEvent, EventType, Family, LoadQuery, LoadedEvent, TypeName,
};

/// The event type a clause appends when it is not itself about event types.
pub const RECORDED: EventType =
    EventType::new(Family::new("sqlx.contract"), TypeName::new("Recorded"));

/// The instant every event carries unless a clause supplies its own.
pub const OCCURRED_AT_MICROSECOND: i64 = 1_700_000_000_123_456;

pub fn fixed_instant() -> Timestamp {
    Timestamp::from_microsecond(OCCURRED_AT_MICROSECOND)
        .expect("a microsecond count inside the supported range is a timestamp")
}

pub fn recorded(sequence: u64, payload: &'static [u8]) -> AppendingEvent {
    event(sequence, RECORDED, payload, fixed_instant())
}

pub fn typed(sequence: u64, event_type: EventType, payload: &'static [u8]) -> AppendingEvent {
    event(sequence, event_type, payload, fixed_instant())
}

pub fn at(sequence: u64, payload: &'static [u8], occurred_at: Timestamp) -> AppendingEvent {
    event(sequence, RECORDED, payload, occurred_at)
}

fn event(
    sequence: u64,
    event_type: EventType,
    payload: &'static [u8],
    occurred_at: Timestamp,
) -> AppendingEvent {
    AppendingEvent {
        sequence,
        event_type,
        payload: Bytes::from_static(payload),
        occurred_at,
    }
}

/// Every observable field of everything the store holds, in the order a load
/// serves it.
///
/// `LoadedEvent` does not derive `PartialEq`, so the comparison a refusal is
/// judged by is spelled out here rather than by widening what the persistence
/// crate publishes.
#[derive(Debug, PartialEq)]
pub struct Stored {
    stream_key: String,
    sequence: u64,
    global_sequence: u64,
    event_type: EventType,
    payload: Bytes,
    occurred_at: Timestamp,
}

pub async fn content(store: &Arc<dyn EventStore>) -> Vec<Stored> {
    observe(store, LoadQuery::default())
        .await
        .into_iter()
        .map(|event| Stored {
            stream_key: event.stream_key,
            sequence: event.sequence,
            global_sequence: event.global_sequence,
            event_type: event.event_type,
            payload: event.payload,
            occurred_at: event.occurred_at,
        })
        .collect()
}

pub async fn observe(store: &Arc<dyn EventStore>, query: LoadQuery) -> Vec<LoadedEvent> {
    let stream = store
        .load(query)
        .await
        .unwrap_or_else(|failure| panic!("the store failed a load outright: {failure}"));
    stream
        .try_collect()
        .await
        .unwrap_or_else(|failure| panic!("the store failed part-way through a stream: {failure}"))
}

pub fn payloads(events: &[LoadedEvent]) -> Vec<&[u8]> {
    events.iter().map(|event| event.payload.as_ref()).collect()
}

/// An append a clause requires to succeed.
pub async fn committed(
    store: &Arc<dyn EventStore>,
    key: &str,
    events: Vec<AppendingEvent>,
) -> AppendOutcome {
    let sequences: Vec<u64> = events.iter().map(|event| event.sequence).collect();
    store.append(key, events).await.unwrap_or_else(|refusal| {
        panic!("appending the sequences {sequences:?} to {key:?} was refused with {refusal}")
    })
}

/// An append a clause requires to be refused as a sequence conflict.
pub async fn conflicted(
    store: &Arc<dyn EventStore>,
    key: &str,
    events: Vec<AppendingEvent>,
    what: &str,
) {
    match store.append(key, events).await {
        Err(AppendError::SequenceConflict(_)) => {}
        Err(other) => panic!("{what} is a sequence conflict, and the store reported {other}"),
        Ok(taken) => panic!(
            "{what} is a sequence conflict, and the store took it, assigning {:?}",
            taken.assigned_sequences
        ),
    }
}

/// Where an append left the numbering off.
pub fn highest(outcome: AppendOutcome) -> u64 {
    outcome
        .assigned_sequences
        .last()
        .copied()
        .expect("an append of at least one event is assigned at least one global_sequence")
}

/// Check that `outcome` picks the numbering up where `last` left it, and return
/// where it leaves off in turn.
pub fn continues_from(last: u64, refused: &str, outcome: AppendOutcome) -> u64 {
    let assigned = outcome.assigned_sequences;
    let expected: Vec<u64> = (1..=assigned.len() as u64)
        .map(|step| last + step)
        .collect();
    assert_eq!(
        assigned, expected,
        "the numbering had reached {last} when {refused} was refused, and the next append to \
         succeed — on a stream of its own — was assigned {assigned:?} where it continues at \
         {expected:?}"
    );
    last + assigned.len() as u64
}
