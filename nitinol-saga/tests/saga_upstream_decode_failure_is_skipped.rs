//! An upstream record whose bytes cannot be decoded is reported and skipped.
//! That is the whole of it — there is no other behaviour to configure.
//!
//! ```gherkin
//!   Scenario: a corrupt record is skipped, standalone
//!     Given an upstream stream whose first record cannot be decoded
//!     When the saga runs
//!     Then the next record is handled — the cursor advanced past the corrupt one
//!     And nothing durable is written for the corrupt one
//!
//!   Scenario: a corrupt record is skipped, under a manager
//!     Given the same stream behind a saga manager
//!     When the manager runs
//!     Then the instance handles the next record
//!     And nothing durable is written for the corrupt one on that instance's stream
//! ```
//!
//! ## Why skipping, and why only skipping
//!
//! A decode failure is deterministic and permanent: the same bytes fail the
//! same way every time, so retrying is pointless, and there is no typed event
//! for `Saga::correlate` to name an owner from, so a record that cannot be
//! decoded cannot be attributed to any saga instance either.  Reporting it and
//! moving the cursor past it is the only action left — and being the only one,
//! it is not a setting.
//!
//! Not even a position is written for the skipped record, which makes a corrupt
//! record the one input that leaves *no* row at all: an input `Saga::handle`
//! refuses still gets its position (see
//! `saga_handle_error_settles_with_position_only.rs`), because a decision was
//! made about it.  Here nothing was decided — there was no typed event to
//! decide from — so there is no decision for a position to be as durable as.
//! The visible consequence is that a restart re-reads the corrupt record and
//! reports it again, which is cheaper than a write per corrupt byte.
//!
//! The report itself reaches an operator as a `tracing::error!`.  That is not a
//! durable record, so it is not something this file can read off a stream and
//! not something it claims to pin; what it does pin is that the *absence* of a
//! durable record is not an absence of progress.
//!
//! ## API contract assumed by this file
//!
//! - `#[derive(SagaSubscribe)]`, `Saga::Subscribe`,
//!   `subscribed_to::<E>(Subscription)` on both `SagaProps` and
//!   `SagaManagerProps`, and `Fn(SagaComponents<S>) -> S` producers — see
//!   `saga_multi_source_observation.rs`.
//!
//! Run with:
//! `cargo test -p nitinol-saga --test saga_upstream_decode_failure_is_skipped`.

#[path = "common/helpers.rs"]
mod common;
use common::{readable_consumed_fact_of, ConsumedFactPayload, JsonCodec, CONSUMED_MARKER};

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};

use nitinol::saga::SagaSubscribe;
use nitinol_eventsource::Event;
use nitinol_persistence::__private::reserved_family;
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::{
    AggregateId, AppendingEvent, EventType, Family, LoadQuery, LoadedEvent, TypeName,
};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{
    Saga, SagaContext, SagaEffect, SagaId, SagaManagerProps, SagaProps, Subscription,
};

// Domain

#[derive(Clone, Debug, Serialize, Deserialize)]
struct OrderPlaced {
    order: String,
}

impl Event for OrderPlaced {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("decode_skip"), TypeName::new("OrderPlaced"));
    type Codec = JsonCodec;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Noted {
    order: String,
}

impl Event for Noted {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("decode_skip"), TypeName::new("Noted"));
    type Codec = JsonCodec;
}

#[derive(SagaSubscribe)]
enum Incoming {
    OrderPlaced(OrderPlaced),
}

const SAGA_ID: &str = "decode-skip-saga";

#[derive(Clone, Debug, PartialEq, Eq)]
struct Seen {
    order: String,
    sequence: u64,
}

type Handled = Arc<Mutex<Vec<Seen>>>;

struct SkippingSaga {
    handled: Handled,
}

#[async_trait]
impl Saga for SkippingSaga {
    type Subscribe = Incoming;
    type Event = Noted;
    type Error = std::convert::Infallible;

    fn correlate(_input: &Self::Subscribe) -> Option<SagaId> {
        Some(SagaId::new(SAGA_ID))
    }

    async fn handle(
        &mut self,
        input: Self::Subscribe,
        ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        let Incoming::OrderPlaced(event) = input;
        self.handled
            .lock()
            .expect("handled buffer must not be poisoned")
            .push(Seen {
                order: event.order.clone(),
                sequence: ctx.upstream_sequence(),
            });
        Ok(SagaEffect::persist(Noted { order: event.order }))
    }
}

// Helpers

async fn append_order_placed(
    store: &Arc<dyn EventStore>,
    stream: &AggregateId,
    sequence: u64,
    order: &str,
) {
    append_raw(
        store,
        stream,
        sequence,
        serde_json::to_vec(&OrderPlaced {
            order: order.to_owned(),
        })
        .map(Bytes::from)
        .expect("encode OrderPlaced must succeed"),
    )
    .await;
}

/// A record carrying the declared event type and bytes its codec cannot read —
/// the same shape a corrupted wire record has.
async fn append_corrupt(store: &Arc<dyn EventStore>, stream: &AggregateId, sequence: u64) {
    append_raw(
        store,
        stream,
        sequence,
        Bytes::from_static(b"NOT-VALID-JSON"),
    )
    .await;
}

async fn append_raw(
    store: &Arc<dyn EventStore>,
    stream: &AggregateId,
    sequence: u64,
    payload: Bytes,
) {
    store
        .append(
            stream.as_str(),
            vec![AppendingEvent {
                sequence,
                event_type: OrderPlaced::EVENT_TYPE,
                payload,
                occurred_at: jiff::Timestamp::now(),
            }],
        )
        .await
        .expect("appending an upstream record must succeed");
}

async fn load_saga_events(store: &Arc<dyn EventStore>, saga_id: &SagaId) -> Vec<LoadedEvent> {
    store
        .load(LoadQuery::by_stream(saga_id))
        .await
        .expect("load saga stream must succeed")
        .try_collect()
        .await
        .expect("collect saga events must succeed")
}

fn positions(events: &[LoadedEvent]) -> Vec<ConsumedFactPayload> {
    events
        .iter()
        .filter_map(readable_consumed_fact_of)
        .collect()
}

/// Every row of the framework's own namespace that is *not* a consumed fact.
///
/// A corrupt record used to leave a durable trace of its own, so the negative
/// claim is made row by row rather than against one retired identity: a trace
/// written under a new name, or alongside the position of a record that *was*
/// readable, is what this is here to catch.
fn unexpected_framework_rows(events: &[LoadedEvent]) -> Vec<String> {
    let reserved = reserved_family("nitinol.saga");
    events
        .iter()
        .filter(|e| e.event_type.family() == reserved && e.event_type != CONSUMED_MARKER)
        .map(|e| format!("{}@{}", e.event_type, e.sequence))
        .collect()
}

fn describe(events: &[LoadedEvent]) -> Vec<String> {
    events
        .iter()
        .map(|e| format!("{}@{}", e.event_type, e.sequence))
        .collect()
}

const BARRIER_TIMEOUT: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(25);

/// The upstream sequences the two seeded records sit at: the corrupt one, then
/// the readable one behind it.
const CORRUPT_SEQUENCE: u64 = 1;
const READABLE_SEQUENCE: u64 = 2;

async fn wait_for_handled(handled: &Handled, expected: usize) -> Vec<Seen> {
    let deadline = Instant::now() + BARRIER_TIMEOUT;
    loop {
        let current = handled
            .lock()
            .expect("handled buffer must not be poisoned")
            .clone();
        if current.len() >= expected {
            return current;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {expected} handle invocations; saw {current:?}"
        );
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// The upstream every test here reads: a corrupt record, then a readable one.
///
/// The readable record is also the barrier.  The poller delivers a stream in
/// ascending order and asks one record at a time, so once seq 2 has been handled
/// the corrupt seq 1's fate is already decided — no sleep settles the negative
/// claims.
async fn seed_upstream(store: &Arc<dyn EventStore>, stream: &AggregateId) {
    append_corrupt(store, stream, CORRUPT_SEQUENCE).await;
    append_order_placed(store, stream, READABLE_SEQUENCE, "ord-2").await;
}

/// The one position a readable record behind the corrupt one is entitled to.
fn position_of_the_readable_record(upstream: &AggregateId) -> ConsumedFactPayload {
    ConsumedFactPayload {
        source: upstream.as_str().to_owned(),
        sequence: READABLE_SEQUENCE,
        source_event_type: OrderPlaced::EVENT_TYPE.to_string(),
    }
}

// Tests

/// Given an upstream stream whose first record cannot be decoded,
/// When a standalone saga runs over it,
/// Then the next record is handled — so the cursor advanced past the corrupt
/// one — and the corrupt record leaves nothing durable behind at all.
#[tokio::test]
async fn a_corrupt_upstream_record_is_skipped_and_writes_nothing() {
    let ps = ProcessSystem::new().await;

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let upstream = AggregateId::new("decode-skip-upstream");
    seed_upstream(&upstream_store, &upstream).await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new(SAGA_ID);
    let handled: Handled = Arc::new(Mutex::new(Vec::new()));

    let handled_for_producer = Arc::clone(&handled);
    let _proxy = SagaProps::<SkippingSaga>::new(
        saga_id.clone(),
        Arc::clone(&saga_store),
        move |_components| SkippingSaga {
            handled: Arc::clone(&handled_for_producer),
        },
    )
    .subscribed_to::<OrderPlaced>(Subscription::stream(&upstream_store, &upstream))
    .spawn(&ps)
    .await;

    let observed = wait_for_handled(&handled, 1).await;
    let events = load_saga_events(&saga_store, &saga_id).await;

    assert_eq!(
        observed,
        vec![Seen {
            order: "ord-2".to_owned(),
            sequence: READABLE_SEQUENCE,
        }],
        "the corrupt record carries no typed event and must never reach \
         `handle`; the next record must — which is also the only evidence the \
         cursor moved past bytes no position was written for"
    );
    assert_eq!(
        positions(&events),
        vec![position_of_the_readable_record(&upstream)],
        "nothing was decided from the corrupt record, so there is no decision \
         for a position to be as durable as — only the handled record's \
         position may be on the stream; it holds {:?}",
        describe(&events)
    );
    assert_eq!(
        unexpected_framework_rows(&events),
        Vec::<String>::new(),
        "a record that cannot be decoded has no owner to attribute it to, so it \
         must leave no durable trace under any framework identity; the stream \
         holds {:?}",
        describe(&events)
    );
}

/// Given the same upstream stream behind a saga manager,
/// When the manager runs,
/// Then the correlated instance handles the next record, and the corrupt one
/// leaves nothing on that instance's stream either.
///
/// Probed separately because the manager is the one place that decides *which*
/// instance an upstream record belongs to, and it decides that from the typed
/// event a corrupt record does not have.  A manager that invented an owner for
/// it would write a row on some instance's stream, and that instance would then
/// carry a record it never consumed.
#[tokio::test]
async fn a_corrupt_upstream_record_is_skipped_under_a_manager_too() {
    let ps = ProcessSystem::new().await;

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let upstream = AggregateId::new("decode-skip-manager-upstream");
    seed_upstream(&upstream_store, &upstream).await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new(SAGA_ID);
    let handled: Handled = Arc::new(Mutex::new(Vec::new()));

    let handled_for_producer = Arc::clone(&handled);
    let _manager =
        SagaManagerProps::<SkippingSaga>::new(Arc::clone(&saga_store), move |_components| {
            SkippingSaga {
                handled: Arc::clone(&handled_for_producer),
            }
        })
        .subscribed_to::<OrderPlaced>(Subscription::stream(&upstream_store, &upstream))
        .spawn(&ps)
        .await;

    let observed = wait_for_handled(&handled, 1).await;
    let events = load_saga_events(&saga_store, &saga_id).await;

    assert_eq!(
        observed,
        vec![Seen {
            order: "ord-2".to_owned(),
            sequence: READABLE_SEQUENCE,
        }],
        "the manager must advance past the corrupt record and route the next one \
         to the instance `correlate` names"
    );
    assert_eq!(
        positions(&events),
        vec![position_of_the_readable_record(&upstream)],
        "the manager must not attribute a corrupt record to any instance, so the \
         only position the instance's stream may carry is the readable record's; \
         it holds {:?}",
        describe(&events)
    );
    assert_eq!(
        unexpected_framework_rows(&events),
        Vec::<String>::new(),
        "no instance's stream may carry a durable trace of a record the manager \
         could not decode an owner from; the stream holds {:?}",
        describe(&events)
    );
}
