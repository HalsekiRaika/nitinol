//! The position of a consumed input is a fact written down under
//! `nitinol.saga.consumed`, and it always names the source it settled.
//!
//! Two things are settled here, and they are the two halves of one change:
//!
//! 1. **The identity.** The declaration is named for the concept — a consumed
//!    fact — rather than for the mechanism the old name described
//!    (`nitinol.saga.consumed_position`). A row under the old path is an
//!    identity no declaration owns, and
//!    `saga_unknown_reserved_record_fails_fast.rs` is what refuses it.
//! 2. **The payload.** A fact states the stream it was read from, the sequence
//!    it reached, *and* the declared source it settled. The third field is not
//!    optional any more: an empty one used to mean "a row from before sources
//!    were named", read as covering every source of its stream. 0.5 keeps no
//!    reader for such a row, so an empty source is a corrupted payload and the
//!    load refuses it.
//!
//! `ConsumedPositionEvent` is `pub(crate)`, so what is named here is the wire
//! form: the identity a journal column holds and the fields its payload
//! carries. That is what an operator and a later incarnation have to go on.
//!
//! The behaviour the fact drives — covering a redelivery, resuming a
//! subscription, one position per declared source — belongs to
//! `saga_consumed_position.rs` and `saga_consumed_position_per_source.rs`.
//!
//! Run with:
//! `cargo test -p nitinol-saga --test saga_consumed_fact_declaration`.

#[path = "common/helpers.rs"]
mod common;
use common::{
    encode_consumed_fact, readable_consumed_fact_of, ConsumedFactPayload, JsonCodec,
    CONSUMED_MARKER,
};

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use nitinol_eventsource::{system::EventSourceSystem, Event};
use nitinol_persistence::__private::reserved_family;
use nitinol_persistence::provider::PersistenceProvider;
use nitinol_persistence::store::{InMemoryEventStore, InMemoryStore, Journal};
use nitinol_persistence::{
    AggregateId, AppendingEvent, EventType, Family, LoadQuery, LoadedEvent, TypeName,
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
        EventType::new(Family::new("consumed_decl"), TypeName::new("OrderPlaced"));
    type Codec = JsonCodec;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ReservationRequested {
    sku: String,
}

impl Event for ReservationRequested {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("consumed_decl"),
        TypeName::new("ReservationRequested"),
    );
    type Codec = JsonCodec;
}

/// Correlation rule of [`RecordingSaga`]: the single process instance every
/// upstream record in this file belongs to.
const SAGA_ID: &str = "consumed-declaration-saga";

/// The whole alphabet this saga reacts to: one observed source, no deadline.
#[derive(SagaSubscribe)]
enum Observed {
    Order(OrderPlaced),
}

struct RecordingSaga {
    handle_count: Arc<AtomicUsize>,
    handled: Arc<Notify>,
}

#[async_trait]
impl Saga for RecordingSaga {
    type Subscribe = Observed;
    type Event = ReservationRequested;
    type Error = std::convert::Infallible;

    fn correlate(_input: &Self::Subscribe) -> Option<SagaId> {
        Some(SagaId::new(SAGA_ID))
    }

    async fn handle(
        &mut self,
        input: Self::Subscribe,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        let Observed::Order(event) = input;
        self.handle_count.fetch_add(1, Ordering::SeqCst);
        self.handled.notify_one();
        Ok(SagaEffect::persist(ReservationRequested { sku: event.sku }))
    }
}

// Helpers

/// The identity consumed facts used to be written down under. `Family::new`
/// refuses the reserved namespace, so naming a framework record goes through
/// the same constructor the framework itself does.
fn released_position_identity() -> EventType {
    EventType::new(
        reserved_family("nitinol.saga"),
        TypeName::new("consumed_position"),
    )
}

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

async fn load_saga_stream(store: &Journal, saga_id: &SagaId) -> Vec<LoadedEvent> {
    store
        .load(LoadQuery::by_stream(saga_id))
        .await
        .expect("load saga stream must succeed")
        .try_collect()
        .await
        .expect("collect saga events must succeed")
}

fn describe(events: &[LoadedEvent]) -> Vec<String> {
    events
        .iter()
        .map(|e| format!("{}@{}", e.event_type, e.sequence))
        .collect()
}

/// A saga, its store and its upstream, with the host already spawned.
struct Running {
    _proxy: nitinol_saga::SagaManagerProxy<RecordingSaga>,
    saga_store: Journal,
    saga_id: SagaId,
    handle_count: Arc<AtomicUsize>,
    handled: Arc<Notify>,
}

/// Spawn the host over `saga_store` with one upstream stream observed, after
/// `seed` has had its chance to put rows on the instance's own stream.
///
/// Every record `correlate` claims here names the one instance [`SAGA_ID`], so
/// what the host spawns on the first arrival is the single resident saga this
/// file reads the facts of.
async fn run(
    upstream_key: &str,
    upstream_records: &[(u64, &str)],
    seed: Option<(EventType, Bytes)>,
) -> Running {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps)
        .with_persistence(PersistenceProvider::new(InMemoryStore::default()))
        .build();

    let upstream_store = Journal::from(InMemoryEventStore::default());
    let upstream = AggregateId::new(upstream_key);
    for (sequence, sku) in upstream_records {
        append_order_placed(&upstream_store, &upstream, *sequence, sku).await;
    }

    let saga_store = Journal::from(InMemoryEventStore::default());
    let saga_id = SagaId::new(SAGA_ID);

    if let Some((event_type, payload)) = seed {
        saga_store
            .append(
                saga_id.as_str(),
                vec![AppendingEvent {
                    sequence: 1,
                    event_type,
                    payload,
                    occurred_at: jiff::Timestamp::now(),
                }],
            )
            .await
            .expect("seeding the saga's own stream must succeed");
    }

    let handle_count = Arc::new(AtomicUsize::new(0));
    let handled = Arc::new(Notify::new());
    let handle_count_for_producer = Arc::clone(&handle_count);
    let handled_for_producer = Arc::clone(&handled);

    let proxy = SagaManagerProps::<RecordingSaga>::new(saga_store.clone(), move |_components| {
        RecordingSaga {
            handle_count: Arc::clone(&handle_count_for_producer),
            handled: Arc::clone(&handled_for_producer),
        }
    })
    .observing::<OrderPlaced>(Subscription::stream(&upstream_store, &upstream))
    .spawn(system.process_system())
    .await;

    Running {
        _proxy: proxy,
        saga_store,
        saga_id,
        handle_count,
        handled,
    }
}

/// How long an instance that *would* act on the waiting record is given to be
/// spawned by the host, replay its own stream and be handed that record. The
/// first test below reaches `handle` well inside this.
const SETTLE: Duration = Duration::from_millis(750);

/// The facts on the saga's own stream, as the rows that can be read as one.
fn facts(events: &[LoadedEvent]) -> Vec<ConsumedFactPayload> {
    events
        .iter()
        .filter_map(readable_consumed_fact_of)
        .collect()
}

// Tests

/// Given one upstream record handled, When the saga's own stream is read back,
/// Then it holds exactly one consumed fact, written down under
/// `nitinol.saga.consumed`, naming the stream it was read from, the sequence it
/// reached and the canonical path of the declared source it settled.
#[tokio::test]
async fn a_handled_input_writes_one_consumed_fact_naming_its_source() {
    let upstream_key = "consumed-decl-orders";
    let running = run(upstream_key, &[(7, "SKU-1")], None).await;

    tokio::time::timeout(SETTLE, running.handled.notified())
        .await
        .expect("the saga must handle the waiting upstream record");

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let stream = loop {
        let stream = load_saga_stream(&running.saga_store, &running.saga_id).await;
        if !facts(&stream).is_empty() {
            break stream;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for a `{CONSUMED_MARKER}` row; the stream holds {:?}",
            describe(&stream)
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    };

    assert_eq!(
        facts(&stream),
        vec![ConsumedFactPayload {
            source: upstream_key.to_owned(),
            sequence: 7,
            source_event_type: OrderPlaced::EVENT_TYPE.to_string(),
        }],
        "a consumed fact names all three: the stream alone cannot say which of \
         two declared sources living on it was settled, and the sequence alone \
         cannot say which stream it belongs to; the stream holds {:?}",
        describe(&stream)
    );
}

/// Given one upstream record handled, When each row of the saga's own stream is
/// examined, Then none of them carries the identity consumed facts used to be
/// written down under.
///
/// Checked row by row rather than by the absence of a string somewhere: a saga
/// writing *both* identities would satisfy the clause above, and this is what
/// refuses it.
#[tokio::test]
async fn a_handled_input_writes_nothing_under_the_released_position_identity() {
    let running = run("consumed-decl-legacy-orders", &[(1, "SKU-2")], None).await;

    tokio::time::timeout(SETTLE, running.handled.notified())
        .await
        .expect("the saga must handle the waiting upstream record");
    tokio::time::sleep(Duration::from_millis(250)).await;

    let stream = load_saga_stream(&running.saga_store, &running.saga_id).await;
    let released = released_position_identity();
    let offending: Vec<String> = stream
        .iter()
        .filter(|e| e.event_type == released)
        .map(|e| format!("{}@{}", e.event_type, e.sequence))
        .collect();

    assert!(
        offending.is_empty(),
        "nothing may be written under `{released}` any more: it is the name of \
         the old mechanism, and a 0.5 reader owns no declaration for it; got \
         {offending:?} on a stream of {:?}",
        describe(&stream)
    );
}

/// Given a consumed fact naming no source, When the saga is loaded, Then the
/// load refuses it — it is not read as a position covering the whole stream.
///
/// The upstream record waiting here sits at a sequence the seeded row does
/// *not* cover, which is what separates the two readings: the whole-stream
/// reading would let the record through to `handle` and write its decision,
/// while the refusal leaves `handle` unreached and the stream exactly as
/// seeded. A record the row *did* cover would be dropped either way and settle
/// nothing.
#[tokio::test]
async fn a_consumed_fact_naming_no_source_refuses_the_load() {
    let upstream_key = "consumed-decl-sourceless-orders";
    let running = run(
        upstream_key,
        &[(2, "SKU-3")],
        Some((CONSUMED_MARKER, encode_consumed_fact(upstream_key, 1, ""))),
    )
    .await;

    tokio::time::sleep(SETTLE).await;

    assert_eq!(
        running.handle_count.load(Ordering::SeqCst),
        0,
        "a fact that cannot say which source it settled is a corrupted payload, \
         not a position covering every source of its stream: 0.5 writes no such \
         row, so the load must refuse it rather than invent a reader for it"
    );

    let stream = load_saga_stream(&running.saga_store, &running.saga_id).await;
    assert_eq!(
        stream.len(),
        1,
        "a refused load decides nothing, so the saga's own stream must still \
         hold exactly the one seeded row; got {:?}",
        describe(&stream)
    );
}
