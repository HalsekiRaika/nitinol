//! Starting a saga up writes nothing. Replay is a fold, never an append.
//!
//! ```gherkin
//!   Scenario: restoring an instance settles no delivery
//!     Given a saga whose decisions and consumed facts are already persisted
//!     When the host restores that instance and it replays
//!     Then replay only folds what the stream already holds: no re-dispatch, no
//!          synthetic marker appended, nothing written to a dead letter queue
//! ```
//!
//! # What is observed
//!
//! Sequence numbers cannot tell "replay wrote a row" from "the first live
//! decision wrote a row", so the batch boundary of each `append` call is
//! recorded directly by a decorating [`RecordingStore`] — the same mechanism
//! `saga_combine_persist_single_append.rs` uses. The seeding appends are made
//! and then forgotten, so what the recorder holds afterwards is exactly what
//! this incarnation wrote.
//!
//! The assertion is that **the first batch this incarnation appends is the one
//! carrying its first live decision**. Waiting for `handle` first is what makes
//! that deterministic rather than a timing guess: the host spawns the instance
//! on the first record its correlation claims, and `on_start` replays the
//! instance's own stream before that record can reach its mailbox — so
//! `handle` cannot run until replay is done, and any batch recorded before the
//! decision batch was written by replay.
//!
//! Two facts the fold must restore are asserted alongside it, because a replay
//! that wrote nothing *and* restored nothing would also pass the count:
//!
//! - **state**: the decision the saga makes carries what `apply` folded out of
//!   the seeded row.
//! - **the consumed position**: the upstream record the seeded fact covers is
//!   never handed to `handle` again, in this incarnation or the next.
//!
//! Run with:
//! `cargo test -p nitinol-saga --test saga_replay_appends_nothing`.

#[path = "common/helpers.rs"]
mod common;
use common::{decode_consumed_fact, encode_consumed_fact, JsonCodec, CONSUMED_MARKER};

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use nitinol_eventsource::{system::EventSourceSystem, Event};
use nitinol_persistence::error::{AppendError, LoadError};
use nitinol_persistence::provider::PersistenceProvider;
use nitinol_persistence::store::{
    EventStore, EventStream, InMemoryEventStore, InMemoryStore, Journal,
};
use nitinol_persistence::{
    AggregateId, AppendOutcome, AppendingEvent, EventType, Family, LoadQuery, LoadedEvent, TypeName,
};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{
    Saga, SagaContext, SagaEffect, SagaId, SagaManagerProps, SagaSubscribe, Subscription,
};

// Domain types

#[derive(Clone, Debug, Serialize, Deserialize)]
struct OrderPlaced {
    sku: String,
}

impl Event for OrderPlaced {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("replay_pure"), TypeName::new("OrderPlaced"));
    type Codec = JsonCodec;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ReservationRequested {
    sku: String,
}

impl Event for ReservationRequested {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("replay_pure"),
        TypeName::new("ReservationRequested"),
    );
    type Codec = JsonCodec;
}

/// Correlation rule of [`FoldingSaga`]: the single process instance every
/// upstream record in this file belongs to.
const SAGA_ID: &str = "replay-pure-saga";

/// What the seeded decision says, and therefore what `apply` must have folded
/// for the live decision below to name it.
const SEEDED_SKU: &str = "SKU-SEEDED";
/// The upstream record the seeded consumed fact covers.
const CONSUMED_SKU: &str = "SKU-ALREADY-CONSUMED";
/// The upstream record this incarnation is the first to see.
const FRESH_SKU: &str = "SKU-FRESH";

/// The whole alphabet this saga reacts to: one observed source, no deadline.
#[derive(SagaSubscribe)]
enum Observed {
    Order(OrderPlaced),
}

/// Folds every decision it has ever made into `reserved`, and names that fold
/// in the next decision it makes. The decision is therefore a readable report
/// of what replay restored.
struct FoldingSaga {
    reserved: Vec<String>,
    handle_count: Arc<AtomicUsize>,
    handled: Arc<Notify>,
}

#[async_trait]
impl Saga for FoldingSaga {
    type Subscribe = Observed;
    type Event = ReservationRequested;
    type Error = std::convert::Infallible;

    fn correlate(_input: &Self::Subscribe) -> Option<SagaId> {
        Some(SagaId::new(SAGA_ID))
    }

    fn apply(&mut self, event: Self::Event) {
        self.reserved.push(event.sku);
    }

    async fn handle(
        &mut self,
        input: Self::Subscribe,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        let Observed::Order(event) = input;
        self.handle_count.fetch_add(1, Ordering::SeqCst);
        self.handled.notify_one();
        Ok(SagaEffect::persist(ReservationRequested {
            sku: format!("[{}]+{}", self.reserved.join(","), event.sku),
        }))
    }
}

// Recording store

#[derive(Clone, Debug)]
struct RecordedEvent {
    event_type: EventType,
    payload: Bytes,
}

type Batches = Arc<Mutex<Vec<Vec<RecordedEvent>>>>;

/// `EventStore` decorator that appends one entry to `batches` per successful
/// `append` call, preserving the events of that call as a group.
#[derive(Clone)]
struct RecordingStore {
    inner: InMemoryEventStore,
    batches: Batches,
}

#[async_trait]
impl EventStore for RecordingStore {
    async fn append(
        &self,
        key: &str,
        events: Vec<AppendingEvent>,
    ) -> Result<AppendOutcome, AppendError> {
        let recorded: Vec<RecordedEvent> = events
            .iter()
            .map(|e| RecordedEvent {
                event_type: e.event_type,
                payload: e.payload.clone(),
            })
            .collect();
        // Record only after the append durably succeeds: a failed attempt never
        // produced a batch boundary in the stream.
        let outcome = self.inner.append(key, events).await?;
        self.batches
            .lock()
            .expect("recording store batch buffer must not be poisoned")
            .push(recorded);
        Ok(outcome)
    }

    async fn load(&self, query: LoadQuery) -> Result<EventStream<'_>, LoadError> {
        self.inner.load(query).await
    }
}

fn snapshot(batches: &Batches) -> Vec<Vec<RecordedEvent>> {
    batches
        .lock()
        .expect("recording store batch buffer must not be poisoned")
        .clone()
}

fn forget(batches: &Batches) {
    batches
        .lock()
        .expect("recording store batch buffer must not be poisoned")
        .clear();
}

fn describe(batches: &[Vec<RecordedEvent>]) -> Vec<Vec<String>> {
    batches
        .iter()
        .map(|batch| {
            batch
                .iter()
                .map(|e| e.event_type.to_string())
                .collect::<Vec<_>>()
        })
        .collect()
}

/// The sku a recorded decision names, or `None` when the row is not one.
fn decided_sku(event: &RecordedEvent) -> Option<String> {
    (event.event_type == ReservationRequested::EVENT_TYPE).then(|| {
        serde_json::from_slice::<ReservationRequested>(&event.payload)
            .expect("a recorded decision must be readable by the codec that wrote it")
            .sku
    })
}

// Helpers

async fn append_order_placed(store: &Journal, upstream: &AggregateId, sequence: u64, sku: &str) {
    let payload = serde_json::to_vec(&OrderPlaced {
        sku: sku.to_owned(),
    })
    .map(Bytes::from)
    .expect("encode OrderPlaced must succeed");
    store
        .append(
            upstream.as_str(),
            vec![AppendingEvent {
                sequence,
                event_type: OrderPlaced::EVENT_TYPE,
                payload,
                occurred_at: jiff::Timestamp::now(),
            }],
        )
        .await
        .expect("append OrderPlaced must succeed");
}

/// Put the two rows a sound 0.5 stream holds after one handled input onto the
/// saga's own stream: the decision, and the consumed fact that closed out the
/// input it was decided from.
async fn seed_one_handled_input(
    store: &Journal,
    saga_id: &SagaId,
    upstream: &AggregateId,
    upstream_sequence: u64,
) {
    let decision = serde_json::to_vec(&ReservationRequested {
        sku: SEEDED_SKU.to_owned(),
    })
    .map(Bytes::from)
    .expect("encode ReservationRequested must succeed");
    store
        .append(
            saga_id.as_str(),
            vec![
                AppendingEvent {
                    sequence: 1,
                    event_type: ReservationRequested::EVENT_TYPE,
                    payload: decision,
                    occurred_at: jiff::Timestamp::now(),
                },
                AppendingEvent {
                    sequence: 2,
                    event_type: CONSUMED_MARKER,
                    payload: encode_consumed_fact(
                        upstream.as_str(),
                        upstream_sequence,
                        &OrderPlaced::EVENT_TYPE.to_string(),
                    ),
                    occurred_at: jiff::Timestamp::now(),
                },
            ],
        )
        .await
        .expect("seeding the saga's own stream must succeed");
}

async fn load_saga_stream(store: &Journal, saga_id: &SagaId) -> Vec<LoadedEvent> {
    store
        .load(LoadQuery::by_stream(saga_id))
        .await
        .expect("load saga stream must succeed")
        .try_collect()
        .await
        .expect("collect saga events must succeed")
}

/// Wait until `batches` holds at least `expected` entries, then let a settle
/// window pass so a batch appended just after would be caught too.
async fn wait_for_batches(batches: &Batches, expected: usize) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while snapshot(batches).len() < expected {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {expected} append batch(es); got {:?}",
            describe(&snapshot(batches))
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    tokio::time::sleep(SETTLE).await;
}

/// How long a replay that *would* write something is given to do it.
const SETTLE: Duration = Duration::from_millis(400);

// Tests

/// Given a saga whose decision and consumed fact are already persisted, When a
/// fresh incarnation starts over that stream and handles its first live
/// record, Then the only batch it ever appended is the one carrying that
/// record's decision, and both the folded state and the consumed position came
/// back with it.
#[tokio::test]
async fn a_restart_appends_nothing_before_its_first_live_decision() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps)
        .with_persistence(PersistenceProvider::new(InMemoryStore::default()))
        .build();

    let upstream_store = Journal::from(InMemoryEventStore::default());
    let upstream = AggregateId::new("replay-pure-orders");
    append_order_placed(&upstream_store, &upstream, 1, CONSUMED_SKU).await;
    append_order_placed(&upstream_store, &upstream, 2, FRESH_SKU).await;

    let batches: Batches = Arc::new(Mutex::new(Vec::new()));
    let saga_store = Journal::from(RecordingStore {
        inner: InMemoryEventStore::default(),
        batches: Arc::clone(&batches),
    });
    let saga_id = SagaId::new(SAGA_ID);

    seed_one_handled_input(&saga_store, &saga_id, &upstream, 1).await;
    // The seeding append is not this incarnation's work.
    forget(&batches);

    let handle_count = Arc::new(AtomicUsize::new(0));
    let handled = Arc::new(Notify::new());
    let handle_count_for_producer = Arc::clone(&handle_count);
    let handled_for_producer = Arc::clone(&handled);

    let _proxy =
        SagaManagerProps::<FoldingSaga>::new(saga_store.clone(), move |_components| FoldingSaga {
            reserved: Vec::new(),
            handle_count: Arc::clone(&handle_count_for_producer),
            handled: Arc::clone(&handled_for_producer),
        })
        .observing::<OrderPlaced>(Subscription::stream(&upstream_store, &upstream))
        .spawn(system.process_system())
        .await;

    // `handle` cannot run before `on_start` has replayed the instance's own
    // stream, so reaching it fixes the point after which any recorded batch is
    // live work.
    tokio::time::timeout(Duration::from_secs(5), handled.notified())
        .await
        .expect("the restored instance must replay and handle the fresh record");
    wait_for_batches(&batches, 1).await;

    let recorded = snapshot(&batches);
    assert_eq!(
        recorded.len(),
        1,
        "the first — and so far only — append of this incarnation must be the \
         batch carrying its first live decision: replay only folds, so a batch \
         before it could only be a re-dispatch, a synthetic marker or a dead \
         letter; got {:?}",
        describe(&recorded)
    );

    let batch = &recorded[0];
    let skus: Vec<String> = batch.iter().filter_map(decided_sku).collect();
    assert_eq!(
        skus,
        vec![format!("[{SEEDED_SKU}]+{FRESH_SKU}")],
        "the live decision must name what `apply` folded out of the seeded row: \
         a replay that wrote nothing because it read nothing would decide from \
         an empty state"
    );

    let positions: Vec<(String, u64)> = batch
        .iter()
        .filter(|e| e.event_type == CONSUMED_MARKER)
        .map(|e| {
            let fact = decode_consumed_fact(&e.payload);
            (fact.source, fact.sequence)
        })
        .collect();
    assert_eq!(
        positions,
        vec![(upstream.as_str().to_owned(), 2)],
        "the decision and the position of the input it settles ride the same \
         append, and the position names the fresh record"
    );

    assert_eq!(
        handle_count.load(Ordering::SeqCst),
        1,
        "the record the seeded consumed fact covers must not reach `handle` \
         again: restoring the position is half of what replay is for"
    );

    let stream = load_saga_stream(&saga_store, &saga_id).await;
    assert_eq!(
        stream
            .iter()
            .map(|e| e.event_type.to_string())
            .collect::<Vec<_>>(),
        vec![
            ReservationRequested::EVENT_TYPE.to_string(),
            CONSUMED_MARKER.to_string(),
            ReservationRequested::EVENT_TYPE.to_string(),
            CONSUMED_MARKER.to_string(),
        ],
        "the stream must hold the two seeded rows and the two this incarnation \
         decided, and nothing else"
    );
}

/// The same stream, with nothing fresh upstream to decide from: an incarnation
/// that finds every record already consumed must append nothing at all.
///
/// The test above fixes "nothing *before* the first decision"; this one fixes
/// "nothing, full stop", which is what rules out a replay whose write is
/// ordered after the first live append rather than suppressed.
#[tokio::test]
async fn a_restart_with_nothing_fresh_upstream_appends_nothing_at_all() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps)
        .with_persistence(PersistenceProvider::new(InMemoryStore::default()))
        .build();

    let upstream_store = Journal::from(InMemoryEventStore::default());
    let upstream = AggregateId::new("replay-quiet-orders");
    append_order_placed(&upstream_store, &upstream, 1, CONSUMED_SKU).await;

    let batches: Batches = Arc::new(Mutex::new(Vec::new()));
    let saga_store = Journal::from(RecordingStore {
        inner: InMemoryEventStore::default(),
        batches: Arc::clone(&batches),
    });
    let saga_id = SagaId::new(SAGA_ID);

    seed_one_handled_input(&saga_store, &saga_id, &upstream, 1).await;
    forget(&batches);

    let handle_count = Arc::new(AtomicUsize::new(0));
    let handled = Arc::new(Notify::new());
    let handle_count_for_producer = Arc::clone(&handle_count);
    let handled_for_producer = Arc::clone(&handled);

    let _proxy =
        SagaManagerProps::<FoldingSaga>::new(saga_store.clone(), move |_components| FoldingSaga {
            reserved: Vec::new(),
            handle_count: Arc::clone(&handle_count_for_producer),
            handled: Arc::clone(&handled_for_producer),
        })
        .observing::<OrderPlaced>(Subscription::stream(&upstream_store, &upstream))
        .spawn(system.process_system())
        .await;

    tokio::time::sleep(Duration::from_millis(750)).await;

    assert_eq!(
        describe(&snapshot(&batches)),
        Vec::<Vec<String>>::new(),
        "a saga that starts up and finds nothing new to decide from must write \
         nothing: starting up is not a decision"
    );
    assert_eq!(
        handle_count.load(Ordering::SeqCst),
        0,
        "every upstream record is already covered by the seeded position, so \
         `handle` has nothing to run for"
    );
}
