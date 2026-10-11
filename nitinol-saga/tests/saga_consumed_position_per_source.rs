//! A consumed position names the declared source it belongs to, not just the
//! upstream stream it was read from.
//!
//! ```gherkin
//!   Scenario: two declared sources share one upstream stream
//!     Given a saga whose two declared sources are written on one stream key
//!     And its own stream records source A consumed up to seq 3
//!     When the saga starts
//!     Then source A's records at or below seq 3 are dropped
//!     And source B's record at seq 2 is still delivered
//!
//!   Scenario: a position the saga writes names its own source
//!     Given a saga that decides on a record of source B
//!     When the decision lands
//!     Then the position it carries names source B's event type
//! ```
//!
//! ## Why the key has to carry the source
//!
//! Two declared sources can live on one upstream stream — an aggregate that
//! emits two event types, a saga observing another saga's stream. Keyed by the
//! stream alone, the higher of the two positions covers the other source's
//! records, so one source's progress silently drops the other's input. That is
//! the same invariant `saga_multi_source_independent_cursors.rs` states for
//! cursors, observed here on the durable position instead.
//!
//! ## API contract assumed by this file
//!
//! - The `nitinol.saga.consumed` record carries the declared source's
//!   event type alongside the stream key and sequence, as proto field 3
//!   (`source_event_type`), written as the event type's canonical path string.
//!   Field 3 is never empty: a row that cannot say which source it settled is a
//!   corrupted payload and refuses the load rather than being read as covering
//!   the whole stream — pinned in
//!   `saga_consumed_fact_declaration.rs::a_consumed_fact_naming_no_source_refuses_the_load`,
//!   which is why nothing here seeds one.
//! - `#[derive(SagaSubscribe)]`, `Saga::Subscribe`, and
//!   `subscribed_to::<E>(Subscription)` — see
//!   `saga_multi_source_observation.rs` for the full assumed surface.
//!
//! Run with:
//! `cargo test -p nitinol-saga --test saga_consumed_position_per_source`.

#[path = "common/helpers.rs"]
mod common;
use common::{JsonCodec, CONSUMED_MARKER};

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};

use nitinol::saga::SagaSubscribe;
use nitinol_eventsource::Event;
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::{
    AggregateId, AppendingEvent, EventType, Family, LoadQuery, LoadedEvent, TypeName,
};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{Saga, SagaContext, SagaEffect, SagaId, SagaProps, Subscription};

// The consumed-position record, mirrored from `proto/consumed.proto`.
//
// `ConsumedPositionEvent` is `pub(crate)` in `nitinol-saga`, so this reads the
// row the way any other consumer of the column does: by decoding the payload's
// `oneof`. Field 3 is what this file is about, which is why the mirror is local
// rather than the one in `common`.

#[derive(Clone, PartialEq, prost::Message)]
struct PerSourceConsumedPayload {
    /// Stream key of the upstream source the position belongs to.
    #[prost(string, tag = "1")]
    source: String,
    /// Highest sequence of that source the saga has consumed.
    #[prost(uint64, tag = "2")]
    sequence: u64,
    /// Canonical path of the declared source's event type.  Empty on a row
    /// written before positions named a source.
    #[prost(string, tag = "3")]
    source_event_type: String,
}

#[derive(Clone, PartialEq, prost::Oneof)]
enum PerSourceConsumedKind {
    #[prost(message, tag = "1")]
    Consumed(PerSourceConsumedPayload),
}

#[derive(Clone, PartialEq, prost::Message)]
struct PerSourceConsumedMarker {
    #[prost(oneof = "PerSourceConsumedKind", tags = "1")]
    kind: Option<PerSourceConsumedKind>,
}

fn encode_consumed_position(source: &str, sequence: u64, source_event_type: &str) -> Bytes {
    let msg = PerSourceConsumedMarker {
        kind: Some(PerSourceConsumedKind::Consumed(PerSourceConsumedPayload {
            source: source.to_owned(),
            sequence,
            source_event_type: source_event_type.to_owned(),
        })),
    };
    Bytes::from(prost::Message::encode_to_vec(&msg))
}

fn consumed_position_of(event: &LoadedEvent) -> Option<PerSourceConsumedPayload> {
    if event.event_type != CONSUMED_MARKER {
        return None;
    }
    <PerSourceConsumedMarker as prost::Message>::decode(&event.payload[..])
        .ok()?
        .kind
        .map(|PerSourceConsumedKind::Consumed(consumed)| consumed)
}

/// The canonical path string a declared source is named by on the wire.
fn source_path(event_type: EventType) -> String {
    event_type.to_path().to_string()
}

// Domain

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Alpha {
    note: String,
}

impl Event for Alpha {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("per_source_position"), TypeName::new("Alpha"));
    type Codec = JsonCodec;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Beta {
    note: String,
}

impl Event for Beta {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("per_source_position"), TypeName::new("Beta"));
    type Codec = JsonCodec;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Noted {
    label: String,
}

impl Event for Noted {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("per_source_position"), TypeName::new("Noted"));
    type Codec = JsonCodec;
}

#[derive(SagaSubscribe)]
enum Shared {
    First(Alpha),
    Second(Beta),
}

const SAGA_ID: &str = "per-source-position-saga";

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Seen {
    variant: &'static str,
    sequence: u64,
}

type Handled = Arc<Mutex<Vec<Seen>>>;

struct SharedStreamSaga {
    handled: Handled,
}

#[async_trait]
impl Saga for SharedStreamSaga {
    type Subscribe = Shared;
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
        let (variant, note) = match input {
            Shared::First(event) => ("First", event.note),
            Shared::Second(event) => ("Second", event.note),
        };
        self.handled
            .lock()
            .expect("handled buffer must not be poisoned")
            .push(Seen {
                variant,
                sequence: ctx.upstream_sequence(),
            });
        Ok(SagaEffect::persist(Noted {
            label: format!("{variant}:{note}"),
        }))
    }
}

// Helpers

async fn append_raw(
    store: &Arc<dyn EventStore>,
    stream_key: &str,
    sequence: u64,
    event_type: EventType,
    payload: Bytes,
) {
    store
        .append(
            stream_key,
            vec![AppendingEvent {
                sequence,
                event_type,
                payload,
                occurred_at: jiff::Timestamp::now(),
            }],
        )
        .await
        .expect("appending a record must succeed");
}

async fn append_alpha(
    store: &Arc<dyn EventStore>,
    stream: &AggregateId,
    sequence: u64,
    note: &str,
) {
    append_raw(
        store,
        stream.as_str(),
        sequence,
        Alpha::EVENT_TYPE,
        serde_json::to_vec(&Alpha {
            note: note.to_owned(),
        })
        .map(Bytes::from)
        .expect("encode Alpha must succeed"),
    )
    .await;
}

async fn append_beta(store: &Arc<dyn EventStore>, stream: &AggregateId, sequence: u64, note: &str) {
    append_raw(
        store,
        stream.as_str(),
        sequence,
        Beta::EVENT_TYPE,
        serde_json::to_vec(&Beta {
            note: note.to_owned(),
        })
        .map(Bytes::from)
        .expect("encode Beta must succeed"),
    )
    .await;
}

/// Stand in for a prior incarnation having recorded a position.
async fn seed_position(
    saga_store: &Arc<dyn EventStore>,
    saga_id: &SagaId,
    row_sequence: u64,
    source: &AggregateId,
    upstream_sequence: u64,
    source_event_type: &str,
) {
    append_raw(
        saga_store,
        saga_id.as_str(),
        row_sequence,
        CONSUMED_MARKER,
        encode_consumed_position(source.as_str(), upstream_sequence, source_event_type),
    )
    .await;
}

const BARRIER_TIMEOUT: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(25);

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

async fn load_saga_events(store: &Arc<dyn EventStore>, saga_id: &SagaId) -> Vec<LoadedEvent> {
    store
        .load(LoadQuery::by_stream(saga_id))
        .await
        .expect("load saga stream must succeed")
        .try_collect()
        .await
        .expect("collect saga events must succeed")
}

fn as_set(observed: &[Seen]) -> Vec<Seen> {
    let mut sorted = observed.to_vec();
    sorted.sort();
    sorted
}

/// Both declared sources read the one stream key `shared`, interleaved as
/// Alpha@1, Beta@2, Alpha@3, Beta@4.
async fn seed_shared_upstream(store: &Arc<dyn EventStore>, shared: &AggregateId) {
    append_alpha(store, shared, 1, "a1").await;
    append_beta(store, shared, 2, "b2").await;
    append_alpha(store, shared, 3, "a3").await;
    append_beta(store, shared, 4, "b4").await;
}

async fn spawn_shared(
    ps: &ProcessSystem,
    saga_store: &Arc<dyn EventStore>,
    upstream_store: &Arc<dyn EventStore>,
    shared: &AggregateId,
    handled: &Handled,
) -> nitinol_saga::SagaProxy<SharedStreamSaga> {
    let handled = Arc::clone(handled);
    SagaProps::<SharedStreamSaga>::new(
        SagaId::new(SAGA_ID),
        Arc::clone(saga_store),
        move |_components| SharedStreamSaga {
            handled: Arc::clone(&handled),
        },
    )
    .subscribed_to::<Alpha>(Subscription::stream(upstream_store, shared))
    .subscribed_to::<Beta>(Subscription::stream(upstream_store, shared))
    .spawn(ps)
    .await
}

// Tests

/// Given two declared sources on one upstream stream, and a position recording
/// source A consumed up to seq 3,
/// When the saga starts,
/// Then A's records at or below 3 are dropped while B's seq 2 is delivered.
///
/// B@4 is the barrier: B's own source delivers ascending, so once 4 has been
/// handled, 2's fate is already decided.
#[tokio::test]
async fn one_sources_position_does_not_cover_another_sources_record() {
    let ps = ProcessSystem::new().await;

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let shared = AggregateId::new("per-source-shared");
    seed_shared_upstream(&upstream_store, &shared).await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new(SAGA_ID);
    seed_position(
        &saga_store,
        &saga_id,
        1,
        &shared,
        3,
        &source_path(Alpha::EVENT_TYPE),
    )
    .await;

    let handled: Handled = Arc::new(Mutex::new(Vec::new()));
    let _proxy = spawn_shared(&ps, &saga_store, &upstream_store, &shared, &handled).await;

    let observed = wait_for_handled(&handled, 2).await;

    assert_eq!(
        as_set(&observed),
        vec![
            Seen {
                variant: "Second",
                sequence: 2
            },
            Seen {
                variant: "Second",
                sequence: 4
            },
        ],
        "a position recorded for source A must drop A's records only: keyed by \
         the stream alone it would also cover source B's seq 2, which no \
         incarnation ever consumed; saw {observed:?}"
    );
}

/// Given a saga that decides on a record of one declared source,
/// When the decision lands,
/// Then the position committed with it names that source's event type, so a
/// later incarnation can tell which source it settled.
#[tokio::test]
async fn the_position_a_decision_carries_names_its_own_source() {
    let ps = ProcessSystem::new().await;

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let shared = AggregateId::new("written-shared");
    append_beta(&upstream_store, &shared, 1, "b1").await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new(SAGA_ID);
    let handled: Handled = Arc::new(Mutex::new(Vec::new()));

    let _proxy = spawn_shared(&ps, &saga_store, &upstream_store, &shared, &handled).await;
    wait_for_handled(&handled, 1).await;

    let events = load_saga_events(&saga_store, &saga_id).await;
    let positions: Vec<PerSourceConsumedPayload> =
        events.iter().filter_map(consumed_position_of).collect();

    assert_eq!(
        positions,
        vec![PerSourceConsumedPayload {
            source: shared.as_str().to_owned(),
            sequence: 1,
            source_event_type: source_path(Beta::EVENT_TYPE),
        }],
        "the position must name the declared source it settled, not only the \
         stream it was read from"
    );
}
