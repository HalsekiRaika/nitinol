//! A row on a saga's own stream that the new model does not recognise stops
//! the load. Nothing is read past it.
//!
//! ```gherkin
//!   Scenario: an unknown nitinol.saga.* record fails fast
//!     Given a stream holding a nitinol.saga.* record the new model does not
//!           recognise — an outbox marker from 0.4, say
//!     When that saga is loaded
//!     Then the load fails hard
//!     And "skip it and carry on" never happens
//! ```
//!
//! The classifier recognises four kinds of row and no more: a domain event
//! (the catch-all), a schedule fact, a consumed fact, and a lifecycle fact.
//! Anything else inside the reserved namespace is a corrupted stream or a
//! version mismatch, and so is a row of a *recognised* declaration whose
//! payload will not decode. Both are refused, which is the same culture as
//! `#[derive(Event)]` refusing to fall back to a default family.
//!
//! # What is observed, and why it is enough
//!
//! `replay` is `pub(crate)`, so the hard error is observed where a user meets
//! it: the instance the host spawns for the waiting record stops in
//! `SagaProcess::on_start` instead of taking it. The manager's `ask` to that
//! instance then fails, so it holds that source's shared cursor and reports the
//! failure — the record is redelivered rather than read past, and no
//! incarnation of this saga ever acts on a history it could not read to the
//! end. Two things are asserted together for every poisoned stream, because
//! either one alone leaves the wrong implementation passing:
//!
//! - **`Saga::handle` is never called**, though a perfectly good upstream
//!   record is sitting there waiting. A classifier that logged the row and
//!   carried on would replay to the end, stay alive and take that record. The
//!   positive control `a_sound_stream_still_handles_the_routed_record` is what
//!   proves the record really was deliverable, so a count of zero cannot pass
//!   for the wrong reason.
//! - **the saga's own stream is untouched** — still exactly the rows the test
//!   seeded. Replay is a fold over that stream and nothing more, so a row
//!   appearing here would mean the load decided something, whether that is a
//!   synthetic marker, a re-dispatch or a dead letter.
//!
//! Run with:
//! `cargo test -p nitinol-saga --test saga_unknown_reserved_record_fails_fast`.

#[path = "common/helpers.rs"]
mod common;
use common::{encode_consumed_fact, JsonCodec, CONSUMED_MARKER, SCHEDULE_MARKER};

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
        EventType::new(Family::new("fail_fast"), TypeName::new("OrderPlaced"));
    type Codec = JsonCodec;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ReservationRequested {
    sku: String,
}

impl Event for ReservationRequested {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("fail_fast"),
        TypeName::new("ReservationRequested"),
    );
    type Codec = JsonCodec;
}

/// Correlation rule of [`RecordingSaga`]: the single process instance every
/// upstream record in this file belongs to.
const SAGA_ID: &str = "fail-fast-saga";

/// The whole alphabet this saga reacts to: one observed source, no deadline.
#[derive(SagaSubscribe)]
enum Observed {
    Order(OrderPlaced),
}

/// Records every `handle` call and never terminates, so whether the waiting
/// record ever reached the saga is observable from outside.
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

/// The framework's own family. `Family::new` refuses the reserved namespace, so
/// a test that has to name a framework record goes through the same constructor
/// the framework itself does.
fn saga_record(type_name: &'static str) -> EventType {
    EventType::new(reserved_family("nitinol.saga"), TypeName::new(type_name))
}

async fn append_order_placed(store: &Journal, upstream: &AggregateId, sequence: u64) {
    let payload = serde_json::to_vec(&OrderPlaced {
        sku: "SKU-1".to_owned(),
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

/// Put one raw row on the saga's own stream at `sequence`, bypassing the
/// framework: the subject of every test here is a row no current writer
/// produces.
async fn seed_raw_row(
    store: &Journal,
    saga_id: &SagaId,
    sequence: u64,
    event_type: EventType,
    payload: Bytes,
) {
    store
        .append(
            saga_id.as_str(),
            vec![AppendingEvent {
                sequence,
                event_type,
                payload,
                occurred_at: jiff::Timestamp::now(),
            }],
        )
        .await
        .expect("seeding a raw row on the saga's own stream must succeed");
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

/// Everything a run of this file needs: the system and the one journal it
/// distributes, the upstream record already waiting, and the handle counter.
struct Harness {
    system: EventSourceSystem<InMemoryStore>,
    /// The journal the upstream stream and the saga's own stream are both
    /// tenants of, kept apart by key.
    journal: Journal,
    upstream: AggregateId,
    saga_id: SagaId,
    handle_count: Arc<AtomicUsize>,
    handled: Arc<Notify>,
}

impl Harness {
    async fn new(upstream_key: &str) -> Self {
        let ps = ProcessSystem::new().await;
        let system = EventSourceSystem::builder(ps)
            .with_persistence(PersistenceProvider::new(InMemoryStore::default()))
            .build();

        let journal = system.persistence().event_store();
        let upstream = AggregateId::new(upstream_key);
        append_order_placed(&journal, &upstream, 1).await;

        Self {
            system,
            journal,
            upstream,
            saga_id: SagaId::new(SAGA_ID),
            handle_count: Arc::new(AtomicUsize::new(0)),
            handled: Arc::new(Notify::new()),
        }
    }

    async fn spawn(&self) -> nitinol_saga::SagaHostProxy<RecordingSaga> {
        let handle_count = Arc::clone(&self.handle_count);
        let handled = Arc::clone(&self.handled);
        SagaManagerProps::<RecordingSaga>::new(move |_components| RecordingSaga {
            handle_count: Arc::clone(&handle_count),
            handled: Arc::clone(&handled),
        })
        .observing::<OrderPlaced>(Subscription::stream(&self.upstream))
        .spawn(&self.system)
        .await
    }
}

/// How long a saga that *would* read its own stream to the end is given to
/// replay and take the waiting record. The positive control below reaches
/// `handle` well inside this, so a zero count after it is a refusal rather than
/// a race.
const SETTLE: Duration = Duration::from_millis(750);

/// Given a poisoned stream, When the host spawns an instance over it, Then
/// `handle` is never reached and the stream is left exactly as it was seeded.
async fn assert_load_refuses(harness: &Harness, seeded: usize, what: &str) {
    let _proxy = harness.spawn().await;

    tokio::time::sleep(SETTLE).await;

    assert_eq!(
        harness.handle_count.load(Ordering::SeqCst),
        0,
        "{what}: the load must fail hard, so the instance stops before taking \
         the waiting record and `Saga::handle` never runs — a classifier that \
         reported the row and carried on would take that record instead"
    );

    let stream = load_saga_stream(&harness.journal, &harness.saga_id).await;
    assert_eq!(
        stream.len(),
        seeded,
        "{what}: a refused load decides nothing, so the saga's own stream must \
         still hold exactly the {seeded} seeded row(s) — no synthetic marker, \
         no position, no durable trace of the refusal; got {:?}",
        describe(&stream)
    );
}

// Tests

/// Positive control. Without it, every zero below could mean the harness
/// simply never delivered anything.
///
/// The seeded rows are the two a sound 0.5 stream holds — a decision and the
/// consumed fact that closed out the input it was decided from — so this also
/// pins that a *recognised* stream is read rather than refused.
#[tokio::test]
async fn a_sound_stream_still_handles_the_routed_record() {
    let harness = Harness::new("fail-fast-sound-orders").await;

    let decision = serde_json::to_vec(&ReservationRequested {
        sku: "SKU-0".to_owned(),
    })
    .map(Bytes::from)
    .expect("encode ReservationRequested must succeed");
    seed_raw_row(
        &harness.journal,
        &harness.saga_id,
        1,
        ReservationRequested::EVENT_TYPE,
        decision,
    )
    .await;
    seed_raw_row(
        &harness.journal,
        &harness.saga_id,
        2,
        CONSUMED_MARKER,
        encode_consumed_fact(
            "fail-fast-sound-orders-earlier",
            1,
            &OrderPlaced::EVENT_TYPE.to_string(),
        ),
    )
    .await;

    let _proxy = harness.spawn().await;

    tokio::time::timeout(SETTLE, harness.handled.notified())
        .await
        .expect("a sound stream must replay and deliver the waiting upstream record");

    assert_eq!(
        harness.handle_count.load(Ordering::SeqCst),
        1,
        "the sound stream must handle exactly the one waiting record"
    );
}

/// Payload that the saga's *own* codec reads without complaint.
///
/// Used under a reserved identity, it separates the two ways a classifier
/// could be written. One that judges the identity refuses the row. One that
/// only ever refuses what fails to decode would send these bytes down the
/// catch-all, read them as a decision of the saga's own and fold them into its
/// state — a 0.4 marker silently becoming domain history.
fn decodes_as_a_domain_event() -> Bytes {
    serde_json::to_vec(&ReservationRequested {
        sku: "SKU-DISGUISED".to_owned(),
    })
    .map(Bytes::from)
    .expect("encode ReservationRequested must succeed")
}

/// A row written by 0.4 — an outbox marker — is inside the reserved namespace
/// and belongs to no declaration the new model has. The load must refuse it on
/// the strength of its identity: these particular bytes decode cleanly as a
/// domain event, so a catch-all that judges only the payload would fold a
/// transport marker into the saga's state.
#[tokio::test]
async fn an_outbox_marker_from_an_earlier_version_refuses_the_load() {
    let harness = Harness::new("fail-fast-outbox-orders").await;

    seed_raw_row(
        &harness.journal,
        &harness.saga_id,
        1,
        saga_record("outbox"),
        decodes_as_a_domain_event(),
    )
    .await;

    assert_load_refuses(&harness, 1, "a 0.4 outbox marker").await;
}

/// The refusal is a judgement about the namespace, not a list of names that
/// once existed: a reserved path this crate has never written must be refused
/// on exactly the same grounds.
#[tokio::test]
async fn an_unheard_of_reserved_row_refuses_the_load() {
    let harness = Harness::new("fail-fast-unheard-orders").await;

    seed_raw_row(
        &harness.journal,
        &harness.saga_id,
        1,
        saga_record("from_a_later_version"),
        decodes_as_a_domain_event(),
    )
    .await;

    assert_load_refuses(&harness, 1, "an unheard-of reserved row").await;
}

/// A dead letter from 0.4 is refused the same way. Named separately because
/// this is the one declaration whose rows the old replay deliberately decoded
/// and threw away, so "recognise it and skip it" is the specific shortcut this
/// test rules out.
#[tokio::test]
async fn a_dead_letter_from_an_earlier_version_refuses_the_load() {
    let harness = Harness::new("fail-fast-dlq-orders").await;

    seed_raw_row(
        &harness.journal,
        &harness.saga_id,
        1,
        saga_record("dead_letter"),
        Bytes::from_static(b"type-less bytes, which is all a dead letter ever was"),
    )
    .await;

    assert_load_refuses(&harness, 1, "a 0.4 dead letter").await;
}

/// A row of a declaration the model *does* recognise, carrying a payload that
/// will not decode, is a corrupted stream entry — refused on the same grounds
/// as an unknown path.
#[tokio::test]
async fn an_undecodable_schedule_fact_refuses_the_load() {
    let harness = Harness::new("fail-fast-schedule-orders").await;

    // A schedule marker with no `oneof` kind: it says nothing about which
    // transition it records, so there is nothing sound to fold.
    seed_raw_row(
        &harness.journal,
        &harness.saga_id,
        1,
        SCHEDULE_MARKER,
        Bytes::new(),
    )
    .await;

    assert_load_refuses(&harness, 1, "an undecodable schedule fact").await;
}

/// The same for a consumed fact: a position that cannot be read says nothing
/// about what was consumed, and reading it as a default would claim an input no
/// incarnation ever decided from.
#[tokio::test]
async fn an_undecodable_consumed_fact_refuses_the_load() {
    let harness = Harness::new("fail-fast-consumed-orders").await;

    seed_raw_row(
        &harness.journal,
        &harness.saga_id,
        1,
        CONSUMED_MARKER,
        Bytes::new(),
    )
    .await;

    assert_load_refuses(&harness, 1, "an undecodable consumed fact").await;
}

/// And for a domain event: a payload the saga's own codec rejects is as
/// poisoned as a framework row, and a replay that skipped it would fold a state
/// missing one of its own decisions.
#[tokio::test]
async fn an_undecodable_domain_event_refuses_the_load() {
    let harness = Harness::new("fail-fast-domain-orders").await;

    seed_raw_row(
        &harness.journal,
        &harness.saga_id,
        1,
        ReservationRequested::EVENT_TYPE,
        Bytes::from_static(b"not json"),
    )
    .await;

    assert_load_refuses(&harness, 1, "an undecodable domain event").await;
}

/// A poisoned row refuses the load wherever it sits: a stream whose sound rows
/// come first must not be read "as far as it got" and then subscribed.
#[tokio::test]
async fn a_poisoned_row_after_a_sound_one_still_refuses_the_load() {
    let harness = Harness::new("fail-fast-trailing-orders").await;

    let decision = serde_json::to_vec(&ReservationRequested {
        sku: "SKU-0".to_owned(),
    })
    .map(Bytes::from)
    .expect("encode ReservationRequested must succeed");
    seed_raw_row(
        &harness.journal,
        &harness.saga_id,
        1,
        ReservationRequested::EVENT_TYPE,
        decision,
    )
    .await;
    seed_raw_row(
        &harness.journal,
        &harness.saga_id,
        2,
        saga_record("outbox"),
        decodes_as_a_domain_event(),
    )
    .await;

    assert_load_refuses(&harness, 2, "a trailing 0.4 outbox marker").await;
}
