//! The resident single-instance saga is expressible as a manager whose
//! correlation is constant.
//!
//! ```gherkin
//!   Scenario: one instance, reached through the one host entry point
//!     Given a saga whose correlate always answers the same SagaId
//!     And it is hosted with one declared source observed
//!     When an order is placed
//!     Then Saga::handle runs once, on the instance that id names
//!     And the instance's own stream holds what a single resident saga writes:
//!       the decision, then the fact settling the input it was decided from
//!     And the wiring says nothing a fanned-out saga's would not: the same
//!       `observing` per declared source and the same instance passivation
//! ```
//!
//! This is the migration path check.  A `Saga::correlate` that always answers
//! the same [`SagaId`] reduces the host to exactly one instance, and what is
//! then observable must be what a single resident saga produces for the same
//! scenario — so the assertions below mirror the saga-side half of
//! `nitinol-saga/tests/e2e_saga.rs::aggregate_event_drives_saga_to_command_target_aggregate`,
//! and a divergence in the instance's identity, in its starting sequence, or in
//! the rows it writes and their order shows up here.
//!
//! The wiring clause is the other half of "not a separate mechanism": the chain
//! below is written with `host` / `observing` / `with_instance_passivation` and
//! nothing else, which is the same chain a saga correlating to many instances
//! is wired with.  A degenerate case that needed a setting of its own — or that
//! could not take one the fanned-out case takes — would not compile here.
//!
//! What a persisted decision then causes elsewhere is a delivery host's doing,
//! and a host reads decisions out of a store without caring which wiring put
//! them there — it keeps no saga resident and cannot tell a managed instance
//! from a resident one.  So it is pinned once, over in `e2e_saga.rs`, and this
//! file stays on the one thing only the manager can get wrong: that constant
//! correlation really does degenerate to a single instance indistinguishable
//! from the resident one.
//!
//! Run with:
//! `cargo test -p nitinol-saga --test saga_manager_degenerate_single_instance`.

#[path = "common/helpers.rs"]
mod common;
use common::{readable_consumed_fact_of, ConsumedFactPayload, JsonCodec, CONSUMED_MARKER};

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};

use nitinol_eventsource::{system::EventSourceSystem, Aggregate, Decider, Decision, Event, Never};
use nitinol_persistence::provider::{CustomStore, PersistenceProvider};
use nitinol_persistence::store::{InMemoryEventStore, InMemoryOffsetStore, Journal};
use nitinol_persistence::{AggregateId, EventType, Family, LoadQuery, LoadedEvent, TypeName};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{
    Saga, SagaContext, SagaEffect, SagaId, SagaSubscribe, SagaSystemExt, Subscription,
};

/// The single instance every upstream event correlates to.  Constant
/// correlation is what makes the manager degenerate to one resident saga.
const RESIDENT_SAGA_ID: &str = "mgr-degenerate-reservation-1";

// Domain types

#[derive(Clone, Debug, Serialize, Deserialize)]
struct OrderPlaced {
    sku: String,
}

impl Event for OrderPlaced {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("mgr.degenerate"), TypeName::new("OrderPlaced"));
    type Codec = JsonCodec;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ReservationRequested {
    sku: String,
}

impl Event for ReservationRequested {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("mgr.degenerate"),
        TypeName::new("ReservationRequested"),
    );
    type Codec = JsonCodec;
}

// Aggregates

#[derive(Default)]
struct Order;

impl Aggregate for Order {
    type Event = OrderPlaced;
    type Recover = Never;

    fn apply(&mut self, _event: OrderPlaced) {}
}

struct PlaceOrder {
    sku: String,
}

impl Decider<PlaceOrder> for Order {
    type Output = ();
    type Rejection = std::convert::Infallible;

    fn decide(&self, cmd: PlaceOrder) -> Decision<OrderPlaced, (), Self::Rejection> {
        Decision::persist(vec![OrderPlaced { sku: cmd.sku }]).output(())
    }
}

// Saga under test

/// The whole alphabet this saga reacts to: one observed source, no deadline.
#[derive(SagaSubscribe)]
enum Observed {
    Order(OrderPlaced),
}

/// What one `Saga::handle` invocation saw.
///
/// The instance it ran on and the sequence that instance was at are the two
/// observations the equivalence rests on: a manager that spawned a differently
/// named instance, or one that handed `handle` a replayed instance's sequence,
/// would not be the resident saga under another name.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CapturedContext {
    saga_id: String,
    sequence: u64,
    sku: String,
}

struct ReservationSaga {
    captured: Arc<Mutex<Vec<CapturedContext>>>,
}

#[async_trait]
impl Saga for ReservationSaga {
    type Subscribe = Observed;
    type Event = ReservationRequested;
    type Error = std::convert::Infallible;

    fn correlate(_input: &Self::Subscribe) -> Option<SagaId> {
        Some(SagaId::new(RESIDENT_SAGA_ID))
    }

    async fn handle(
        &mut self,
        input: Self::Subscribe,
        ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        let Observed::Order(event) = input;
        self.captured
            .lock()
            .expect("captured mutex is never poisoned: no holder panics while the guard is alive")
            .push(CapturedContext {
                saga_id: ctx.saga_id().as_str().to_owned(),
                sequence: ctx.sequence(),
                sku: event.sku.clone(),
            });

        Ok(SagaEffect::persist(ReservationRequested { sku: event.sku }))
    }
}

// Helpers

/// How long a positive claim is given before the test calls it a failure.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// How often a probe re-reads what it is waiting for.
const PROBE_INTERVAL: Duration = Duration::from_millis(25);

/// The passivation window the wiring names.
///
/// Long enough that nothing is passivated inside this test: the clause the
/// setting serves here is that the degenerate case takes it at all, and what
/// passivation then does is pinned in `saga_manager_passivation.rs`.
const INSTANCE_PASSIVATION: Duration = Duration::from_secs(300);

async fn load_saga_events(store: &Journal, saga_id: &SagaId) -> Vec<LoadedEvent> {
    store
        .load(LoadQuery::by_stream(saga_id))
        .await
        .expect("saga event store load must succeed")
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

fn captured_snapshot(captured: &Arc<Mutex<Vec<CapturedContext>>>) -> Vec<CapturedContext> {
    captured
        .lock()
        .expect("captured mutex is never poisoned: no holder panics while the guard is alive")
        .clone()
}

async fn wait_until_handled(
    captured: &Arc<Mutex<Vec<CapturedContext>>>,
    want: usize,
) -> Vec<CapturedContext> {
    let deadline = std::time::Instant::now() + PROBE_TIMEOUT;
    loop {
        let seen = captured_snapshot(captured);
        if seen.len() >= want {
            return seen;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {want} Saga::handle invocations; saw {seen:?}"
        );
        tokio::time::sleep(PROBE_INTERVAL).await;
    }
}

/// The instance's own stream, once it holds the fact settling an input.
///
/// That row is the last of the batch the decision rides in, so waiting for it
/// means the whole append has landed and the stream can be read in full.
async fn wait_until_settled(store: &Journal, saga_id: &SagaId) -> Vec<LoadedEvent> {
    let deadline = std::time::Instant::now() + PROBE_TIMEOUT;
    loop {
        let events = load_saga_events(store, saga_id).await;
        if events.iter().any(|e| e.event_type == CONSUMED_MARKER) {
            return events;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for a `{CONSUMED_MARKER}` row on the instance's \
             own stream; it held {:?}",
            describe(&events)
        );
        tokio::time::sleep(PROBE_INTERVAL).await;
    }
}

// Test

#[tokio::test]
async fn single_constant_correlation_reproduces_the_resident_saga() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps)
        .with_persistence(PersistenceProvider::new(
            CustomStore::builder()
                .eventstore(InMemoryEventStore::default())
                .saga_offsets(InMemoryOffsetStore::default())
                .build(),
        ))
        .build();
    let journal = system.persistence().event_store();

    let order_id = AggregateId::new("mgr-degenerate-order");
    let order_proxy = system.spawn_aggregate::<Order>(order_id.clone()).await;

    let saga_id = SagaId::new(RESIDENT_SAGA_ID);
    let captured: Arc<Mutex<Vec<CapturedContext>>> = Arc::new(Mutex::new(Vec::new()));

    let captured_for_producer = Arc::clone(&captured);

    // The one host entry point, with the one source this alphabet declares
    // bound and the instances' passivation named — the chain a saga correlating
    // to many instances is wired with, written for one that correlates to one.
    let _manager_proxy = system
        .host::<ReservationSaga>(move |_components| ReservationSaga {
            captured: Arc::clone(&captured_for_producer),
        })
        .observing::<OrderPlaced>(Subscription::stream(&order_id))
        .with_instance_passivation(INSTANCE_PASSIVATION)
        .spawn()
        .await;

    order_proxy
        .ask(PlaceOrder {
            sku: "SKU-001".into(),
        })
        .await
        .expect("ask(PlaceOrder) must succeed");

    assert_eq!(
        wait_until_handled(&captured, 1).await,
        vec![CapturedContext {
            saga_id: RESIDENT_SAGA_ID.to_owned(),
            sequence: 0,
            sku: "SKU-001".to_owned(),
        }],
        "the host must route the upstream event to the one instance its \
         constant correlation names, exactly once and at sequence 0 — the \
         instance is indistinguishable from a single resident saga"
    );

    let saga_events = wait_until_settled(&journal, &saga_id).await;

    assert_eq!(
        describe(&saga_events),
        vec![
            format!("{}@1", ReservationRequested::EVENT_TYPE),
            format!("{CONSUMED_MARKER}@2"),
        ],
        "the instance writes what the resident wiring writes and in the same \
         places: the decision at sequence 1 and the fact settling its input \
         closing the batch at sequence 2. A manager that re-numbered the \
         instance's stream, or added bookkeeping of its own to it, would not be \
         a migration path"
    );

    assert_eq!(
        saga_events
            .iter()
            .filter_map(readable_consumed_fact_of)
            .collect::<Vec<_>>(),
        vec![ConsumedFactPayload {
            source: order_id.as_str().to_owned(),
            sequence: 1,
            source_event_type: OrderPlaced::EVENT_TYPE.to_string(),
        }],
        "the position the manager's shared subscription settled is recorded on \
         the instance's own stream, naming the same upstream stream, sequence \
         and declared source a resident saga would have recorded"
    );
}
