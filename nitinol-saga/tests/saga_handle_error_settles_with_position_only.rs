//! An input `Saga::handle` refused is settled by its position alone.
//!
//! With the dead letter queue gone, a `handle` error has no durable record to
//! be written as. What it still has is a *decision about the input*: it was
//! seen, it produced nothing, and it must not come round again. The position
//! is what says that, and in 0.5 it is the only row such an input leaves.
//!
//! The error itself reaches an operator as a `tracing::error!`, which is not a
//! contract this file can observe and is not one it claims to.
//!
//! # The three implementations this separates
//!
//! - **position plus a durable trace** — what 0.4 did: the dead letter and the
//!   position rode one append. `no_dead_letter_row_is_written` refuses it,
//!   checked per row so a trace written alongside the position is caught.
//! - **nothing at all** — the position withheld along with the dead letter.
//!   The input is then never consumed, so the poller brings it back for ever;
//!   `the_refused_input_is_not_redelivered` and
//!   `a_restart_does_not_hand_the_refused_input_back` refuse it from both
//!   sides of a restart.
//! - **position only** — what is asserted here.
//!
//! The remaining branch of the same contract — the position append *itself*
//! failing, which withholds the cursor so the input does come back — is held
//! by `saga_manager_cursor_semantics.rs`
//! (`instance_handler_error_holds_the_shared_cursor`), where a store that
//! rejects every append makes it observable.
//!
//! Run with:
//! `cargo test -p nitinol-saga --test saga_handle_error_settles_with_position_only`.

#[path = "common/helpers.rs"]
mod common;
use common::{readable_consumed_fact_of, ConsumedFactPayload, JsonCodec, CONSUMED_MARKER};

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
use nitinol_persistence::store::{EventStore, InMemoryEventStore, InMemoryStore};
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
        EventType::new(Family::new("handle_err"), TypeName::new("OrderPlaced"));
    type Codec = JsonCodec;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ReservationRequested {
    sku: String,
}

impl Event for ReservationRequested {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("handle_err"),
        TypeName::new("ReservationRequested"),
    );
    type Codec = JsonCodec;
}

#[derive(Debug)]
struct Refused;

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "handle refused the record")
    }
}

impl std::error::Error for Refused {}

/// Correlation rule of both sagas in this file. The restart test hands one
/// saga stream from [`RefusingSaga`] to [`AcceptingSaga`], so the two types
/// must name the same instance for the second incarnation to be the same saga.
const SAGA_ID: &str = "handle-error-saga";

/// The whole alphabet both sagas react to: one observed source, no deadline.
#[derive(SagaSubscribe)]
enum Observed {
    Order(OrderPlaced),
}

/// Refuses every record it is handed.
struct RefusingSaga {
    handle_count: Arc<AtomicUsize>,
    handled: Arc<Notify>,
}

#[async_trait]
impl Saga for RefusingSaga {
    type Subscribe = Observed;
    type Event = ReservationRequested;
    type Error = Refused;

    fn correlate(_input: &Self::Subscribe) -> Option<SagaId> {
        Some(SagaId::new(SAGA_ID))
    }

    async fn handle(
        &mut self,
        _input: Self::Subscribe,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        self.handle_count.fetch_add(1, Ordering::SeqCst);
        self.handled.notify_one();
        Err(Refused)
    }
}

/// Accepts every record. Used as the second incarnation, so "the refused input
/// did not come back" cannot be confused with "this saga refuses everything".
struct AcceptingSaga {
    handle_count: Arc<AtomicUsize>,
    handled: Arc<Notify>,
}

#[async_trait]
impl Saga for AcceptingSaga {
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

/// The identity a durable trace of a failure used to be written down under.
/// `Family::new` refuses the reserved namespace, so naming a framework record
/// goes through the same constructor the framework itself does.
fn released_dead_letter_identity() -> EventType {
    EventType::new(
        reserved_family("nitinol.saga"),
        TypeName::new("dead_letter"),
    )
}

async fn append_order_placed(
    store: &Arc<dyn EventStore>,
    upstream: &AggregateId,
    sequence: u64,
    sku: &str,
) {
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

async fn load_saga_stream(store: &Arc<dyn EventStore>, saga_id: &SagaId) -> Vec<LoadedEvent> {
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

/// How long a saga is given to settle the refused input, and how long a
/// redelivery that would come is given to arrive.
const SETTLE: Duration = Duration::from_millis(750);

/// The upstream sequence the one refused record sits at.
const REFUSED_SEQUENCE: u64 = 4;

struct Refusal {
    /// Kept alive only because dropping a proxy does not stop the process it
    /// addresses. The restart test therefore gives the second incarnation an
    /// upstream store of its own rather than trying to retire this one: the
    /// store this host polls never grows again, so it stays quiet and the two
    /// never decide the same record onto one stream.
    _proxy: nitinol_saga::SagaManagerProxy<RefusingSaga>,
    system: EventSourceSystem<InMemoryStore>,
    saga_store: Arc<dyn EventStore>,
    upstream: AggregateId,
    saga_id: SagaId,
    handle_count: Arc<AtomicUsize>,
    stream: Vec<LoadedEvent>,
}

/// Run one record through a saga that refuses it, and hand back the stream the
/// refusal left behind.
async fn refuse_one_record(upstream_key: &str) -> Refusal {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps)
        .with_persistence(PersistenceProvider::new(InMemoryStore::default()))
        .build();

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let upstream = AggregateId::new(upstream_key);
    append_order_placed(&upstream_store, &upstream, REFUSED_SEQUENCE, "SKU-REFUSED").await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new(SAGA_ID);

    let handle_count = Arc::new(AtomicUsize::new(0));
    let handled = Arc::new(Notify::new());
    let handle_count_for_producer = Arc::clone(&handle_count);
    let handled_for_producer = Arc::clone(&handled);

    let proxy =
        SagaManagerProps::<RefusingSaga>::new(Arc::clone(&saga_store), move |_components| {
            RefusingSaga {
                handle_count: Arc::clone(&handle_count_for_producer),
                handled: Arc::clone(&handled_for_producer),
            }
        })
        .observing::<OrderPlaced>(Subscription::stream(&upstream_store, &upstream))
        .spawn(system.process_system())
        .await;

    tokio::time::timeout(SETTLE, handled.notified())
        .await
        .expect("the saga must be handed the waiting upstream record");
    // Let the settlement append land, and let a redelivery arrive if the
    // implementation under test withholds the cursor.
    tokio::time::sleep(SETTLE).await;

    let stream = load_saga_stream(&saga_store, &saga_id).await;
    Refusal {
        _proxy: proxy,
        system,
        saga_store,
        upstream,
        saga_id,
        handle_count,
        stream,
    }
}

// Tests

/// Given a `handle` that refuses its record, When the saga's own stream is read
/// back, Then it holds exactly one row: the position of the refused input.
#[tokio::test]
async fn the_refused_input_leaves_only_its_position() {
    let refusal = refuse_one_record("handle-err-orders").await;

    assert_eq!(
        refusal
            .stream
            .iter()
            .map(|e| e.event_type.to_string())
            .collect::<Vec<_>>(),
        vec![CONSUMED_MARKER.to_string()],
        "a refused input produces no decision, so the only thing left to record \
         is that it was consumed; the stream holds {:?}",
        describe(&refusal.stream)
    );

    assert_eq!(
        refusal
            .stream
            .iter()
            .filter_map(readable_consumed_fact_of)
            .collect::<Vec<_>>(),
        vec![ConsumedFactPayload {
            source: refusal.upstream.as_str().to_owned(),
            sequence: REFUSED_SEQUENCE,
            source_event_type: OrderPlaced::EVENT_TYPE.to_string(),
        }],
        "the position must name the record that was refused, not a default: a \
         position of the wrong sequence would either re-open it or swallow the \
         records behind it"
    );
}

/// Given a `handle` that refuses its record, When each row of the saga's own
/// stream is examined, Then none of them carries the identity a durable trace
/// of the failure used to be written down under.
///
/// Checked row by row rather than by the absence of a string somewhere: the
/// 0.4 behaviour wrote the trace *and* the position on one append, which the
/// clause above would already catch — but a trace on a separate append, or
/// under a renamed identity, is what this refuses on its own terms.
#[tokio::test]
async fn no_dead_letter_row_is_written() {
    let refusal = refuse_one_record("handle-err-dlq-orders").await;

    let released = released_dead_letter_identity();
    let reserved = reserved_family("nitinol.saga");
    let offending: Vec<String> = refusal
        .stream
        .iter()
        .filter(|e| {
            e.event_type == released
                // Anything else of the framework's own that is not the position
                // would be a trace under another name.
                || (e.event_type.family() == reserved && e.event_type != CONSUMED_MARKER)
        })
        .map(|e| format!("{}@{}", e.event_type, e.sequence))
        .collect();

    assert!(
        offending.is_empty(),
        "a refused input gets no durable trace, under `{released}` or any other \
         framework identity: a store of type-less bytes is withdrawn, not \
         renamed; got {offending:?} on a stream of {:?}",
        describe(&refusal.stream)
    );
}

/// Given the refused input settled by its position, When the poller carries on,
/// Then the record is not handed to `handle` again.
///
/// This is the half that refuses "write nothing": without a position the input
/// is never consumed, so the cursor never passes it.
#[tokio::test]
async fn the_refused_input_is_not_redelivered() {
    let refusal = refuse_one_record("handle-err-redelivery-orders").await;

    assert_eq!(
        refusal.handle_count.load(Ordering::SeqCst),
        1,
        "the position settles the input, so the poller moves past it: a refusal \
         that recorded nothing would bring the same record round for ever"
    );
}

/// Given a saga whose only input was refused, When a fresh incarnation starts
/// over the same saga stream, Then the refused record is not handed to it
/// either — while a record behind it still is.
///
/// The second incarnation accepts everything it is handed, so the count it
/// reports separates "the position covered the refused record" from "this saga
/// refuses everything". The record behind it is the positive control: without
/// one, a count of zero could mean the subscription was never wired.
///
/// The second incarnation reads an upstream store of its own, holding both
/// records under the same stream key. That is what keeps the first incarnation
/// — which no proxy drop can retire — out of the way: its own store never
/// grows again, so only one process ever decides the second record.
#[tokio::test]
async fn a_restart_does_not_hand_the_refused_input_back() {
    let refusal = refuse_one_record("handle-err-restart-orders").await;

    let restarted_upstream: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    append_order_placed(
        &restarted_upstream,
        &refusal.upstream,
        REFUSED_SEQUENCE,
        "SKU-REFUSED",
    )
    .await;
    append_order_placed(
        &restarted_upstream,
        &refusal.upstream,
        REFUSED_SEQUENCE + 1,
        "SKU-AFTER",
    )
    .await;

    let handle_count = Arc::new(AtomicUsize::new(0));
    let handled = Arc::new(Notify::new());
    let handle_count_for_producer = Arc::clone(&handle_count);
    let handled_for_producer = Arc::clone(&handled);

    let _second = SagaManagerProps::<AcceptingSaga>::new(
        Arc::clone(&refusal.saga_store),
        move |_components| AcceptingSaga {
            handle_count: Arc::clone(&handle_count_for_producer),
            handled: Arc::clone(&handled_for_producer),
        },
    )
    .observing::<OrderPlaced>(Subscription::stream(&restarted_upstream, &refusal.upstream))
    .spawn(refusal.system.process_system())
    .await;

    tokio::time::timeout(SETTLE, handled.notified())
        .await
        .expect("the second incarnation must be handed the record behind the refused one");
    tokio::time::sleep(SETTLE).await;

    assert_eq!(
        handle_count.load(Ordering::SeqCst),
        1,
        "exactly the record behind the refused one reaches the new incarnation: \
         the refused record's position is durable, so no restart re-opens it"
    );

    let stream = load_saga_stream(&refusal.saga_store, &refusal.saga_id).await;
    assert_eq!(
        stream
            .iter()
            .map(|e| e.event_type.to_string())
            .collect::<Vec<_>>(),
        vec![
            CONSUMED_MARKER.to_string(),
            ReservationRequested::EVENT_TYPE.to_string(),
            CONSUMED_MARKER.to_string(),
        ],
        "the stream holds the refused input's position and then the next \
         input's decision and position — one record refused, one accepted, no \
         trace of either failure beyond the positions"
    );
}
