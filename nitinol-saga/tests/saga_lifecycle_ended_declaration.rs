//! `Ended` is a lifecycle fact of its own, written down under
//! `nitinol.saga.lifecycle`.
//!
//! It used to share an envelope with the outbox: the same
//! `nitinol.saga.outbox` identity carried `tell_requested`, `tell_acked`,
//! `tell_failed`, `scheduled` *and* `ended`, so "this saga terminated" could
//! only be read by a reader that also knew a transport mechanism's grammar.
//! With the transport gone the fact needs — and gets — a declaration of its
//! own.
//!
//! # What is observed
//!
//! `LifecycleEvent` is `pub(crate)`, so what is named here is the wire form:
//! the identity a journal column holds and the `oneof` arm its payload
//! carries. That pair is the whole of what an operator, a registry or a later
//! incarnation has to go on, and it is the part the framework cannot change
//! without changing what an existing journal means.
//!
//! Three clauses, each needed:
//!
//! - the identity is `nitinol.saga.lifecycle` and the payload's arm is
//!   `ended` — the positive form;
//! - no row of the terminated stream sits under `nitinol.saga.outbox`,
//!   checked per row rather than by the absence of a string anywhere, so a
//!   marker written *alongside* the lifecycle fact would be caught;
//! - the fact rides the same append as the decision and the consumed position
//!   of the input that closed the saga, which is what stops a crash between
//!   two appends from leaving a terminated saga that does not look terminated.
//!
//! That the fact *is* the restart guard is
//! `saga_terminated_skip_on_restart.rs`; that the path is collected by the
//! registry is `saga_events_are_link_collected.rs`.
//!
//! Run with:
//! `cargo test -p nitinol-saga --test saga_lifecycle_ended_declaration`.

#[path = "common/helpers.rs"]
mod common;
use common::{lifecycle_fact, lifecycle_kind_of, JsonCodec, CONSUMED_MARKER, LIFECYCLE_MARKER};

use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};

use nitinol_eventsource::{system::EventSourceSystem, Event};
use nitinol_persistence::__private::reserved_family;
use nitinol_persistence::provider::PersistenceProvider;
use nitinol_persistence::store::{InMemoryStore, Journal};
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
        EventType::new(Family::new("lifecycle_decl"), TypeName::new("OrderPlaced"));
    type Codec = JsonCodec;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ReservationRequested {
    sku: String,
}

impl Event for ReservationRequested {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("lifecycle_decl"),
        TypeName::new("ReservationRequested"),
    );
    type Codec = JsonCodec;
}

/// Correlation rule of [`EndOnFirstSaga`]: the single process instance every
/// upstream record in this file belongs to.
const SAGA_ID: &str = "lifecycle-declaration-saga";

/// The whole alphabet this saga reacts to: one observed source, no deadline.
#[derive(SagaSubscribe)]
enum Observed {
    Order(OrderPlaced),
}

/// Persists one decision and terminates in the same breath, so the stream it
/// leaves behind holds every row an `End` produces and nothing else.
struct EndOnFirstSaga;

#[async_trait]
impl Saga for EndOnFirstSaga {
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
        Ok(SagaEffect::persist(ReservationRequested { sku: event.sku }).then_end())
    }
}

// Helpers

/// The outbox identity `Ended` used to share. `Family::new` refuses the
/// reserved namespace, so naming a framework record goes through the same
/// constructor the framework itself does.
fn released_outbox_identity() -> EventType {
    EventType::new(reserved_family("nitinol.saga"), TypeName::new("outbox"))
}

async fn append_order_placed(store: &Journal, upstream: &AggregateId, sku: &str) {
    let payload = serde_json::to_vec(&OrderPlaced {
        sku: sku.to_owned(),
    })
    .map(Bytes::from)
    .expect("encode OrderPlaced must succeed");
    store
        .append(
            upstream.as_str(),
            vec![AppendingEvent {
                sequence: 1,
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

/// Run one saga to termination over a fresh store and hand back the stream it
/// left behind.
async fn stream_after_one_end(upstream_key: &str, sku: &str) -> Vec<LoadedEvent> {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps)
        .with_persistence(PersistenceProvider::new(InMemoryStore::default()))
        .build();

    // One journal: the order stream the saga observes and the saga's own stream
    // are tenants of the store the system distributes, kept apart by stream key.
    let journal = system.persistence().event_store();
    let upstream = AggregateId::new(upstream_key);
    append_order_placed(&journal, &upstream, sku).await;

    let saga_id = SagaId::new(SAGA_ID);

    let _host = SagaManagerProps::<EndOnFirstSaga>::new(|_components| EndOnFirstSaga)
        .observing::<OrderPlaced>(Subscription::stream(&upstream))
        .spawn(&system)
        .await;

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let events = load_saga_stream(&journal, &saga_id).await;
        if events.iter().any(|e| lifecycle_kind_of(e).is_some()) {
            // Let anything the interpreter might still write land, so a row
            // appended just after the fact is not missed.
            tokio::time::sleep(Duration::from_millis(250)).await;
            return load_saga_stream(&journal, &saga_id).await;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for `End` to persist a `nitinol.saga.lifecycle` \
             row; the stream holds {:?}",
            describe(&events)
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

// Tests

/// Given a saga that reaches `End`, When its own stream is read back, Then
/// exactly one row carries the identity `nitinol.saga.lifecycle` and its
/// payload's `oneof` arm is `ended`.
#[tokio::test]
async fn end_writes_one_lifecycle_row_whose_payload_names_the_ended_arm() {
    let stream = stream_after_one_end("lifecycle-decl-orders", "SKU-1").await;

    let facts: Vec<&'static str> = stream
        .iter()
        .filter_map(lifecycle_kind_of)
        .map(|kind| lifecycle_fact(&kind))
        .collect();

    assert_eq!(
        facts,
        vec!["ended"],
        "`End` must write exactly one lifecycle row, and the fact it states \
         lives in the payload's `oneof` — the column names the declaration, \
         nothing more; the stream holds {:?}",
        describe(&stream)
    );
    assert_eq!(
        LIFECYCLE_MARKER.to_string(),
        "nitinol.saga.lifecycle",
        "the identity a terminated saga is recognised by is the one this file \
         seeds and reads: a change here changes what every existing journal means"
    );
}

/// Given a saga that reaches `End`, When each row of its stream is examined,
/// Then none of them carries the outbox identity the fact used to share.
///
/// Checked row by row rather than by the absence of a string somewhere in the
/// output: a lifecycle row written *and* an outbox marker written alongside it
/// would satisfy the clause above, and this is what refuses it.
#[tokio::test]
async fn a_terminated_stream_holds_no_outbox_row() {
    let stream = stream_after_one_end("lifecycle-decl-outbox-orders", "SKU-2").await;

    let released = released_outbox_identity();
    let offending: Vec<String> = stream
        .iter()
        .filter(|e| e.event_type == released)
        .map(|e| format!("{}@{}", e.event_type, e.sequence))
        .collect();

    assert!(
        offending.is_empty(),
        "nothing may be written under `{released}` any more: the envelope that \
         carried `Ended` alongside a transport mechanism's transitions is gone, \
         and a row under it would be an identity no declaration owns; got \
         {offending:?} on a stream of {:?}",
        describe(&stream)
    );
}

/// Given the one input that closed the saga, When the rows it produced are
/// read, Then the decision, the lifecycle fact and the consumed position all
/// sit on one atomic append.
///
/// They are consecutive sequences either way, so the grouping is what
/// distinguishes it: a crash between two appends would otherwise leave a saga
/// whose decision is durable and whose termination is not, or a position
/// recorded for an input whose `End` never landed.
#[tokio::test]
async fn the_lifecycle_fact_rides_the_append_that_closes_the_input() {
    let stream = stream_after_one_end("lifecycle-decl-atomic-orders", "SKU-3").await;

    // `InMemoryEventStore` keeps no batch boundary, so the append group is read
    // off the one thing it does keep: rows of a single append occupy
    // consecutive sequences claimed together, starting at 1 for a fresh saga.
    // The order *within* the group is not part of the contract, so only the
    // membership and the gap-free run are asserted.
    assert_eq!(
        stream.iter().map(|e| e.sequence).collect::<Vec<_>>(),
        vec![1, 2, 3],
        "one input closing a saga produces three rows on one gap-free run of \
         sequences: a gap would mean a sequence was claimed for a row that \
         never landed, which is the split-append window this invariant closes; \
         the stream holds {:?}",
        describe(&stream)
    );

    let mut identities: Vec<String> = stream
        .iter()
        .map(|e| e.event_type.to_string())
        .collect::<Vec<_>>();
    identities.sort();
    let mut expected = vec![
        ReservationRequested::EVENT_TYPE.to_string(),
        LIFECYCLE_MARKER.to_string(),
        CONSUMED_MARKER.to_string(),
    ];
    expected.sort();
    assert_eq!(
        identities, expected,
        "the run holds the decision, the lifecycle fact and the position of the \
         input they settle — one atomic append, so a crash can never leave a \
         saga whose decision is durable and whose termination is not"
    );
}
