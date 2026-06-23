//! Mock-target integration tests for the new Outbox-backed Persist+Tell.
//!
//! Verifies:
//! - `SagaEffect::tell(target, cmd)` builds the tell-shaped Persist branch
//! - The interpreter dispatches the command to the (mock) target asynchronously
//! - The Reserve command must implement `Clone` because the retry executor
//!   re-`tell`s with a cloned copy each attempt
//!
//! Reserved-prefix invariant (`nitinol.saga.outbox.*`) is asserted separately
//! in `saga_outbox_persist_atomicity.rs`.

mod common;
use common::{shape_of, JsonCodec, Shape};

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use nitinol_eventsource::test_helpers::MockAggregateProxy;
use nitinol_eventsource::{
    system::EventSourceSystem, Aggregate, Context, Decider, Effect, Event, SequenceCursor,
};
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::{AggregateId, AppendingEvent, EventType};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{Saga, SagaContext, SagaEffect, SagaId, SagaProps};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
struct OrderPlaced {
    sku: String,
}

impl Event for OrderPlaced {
    const EVENT_TYPE: EventType = EventType::from_str("saga.tell_mock.OrderPlaced");
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct ReservationRequested {
    sku: String,
}

impl Event for ReservationRequested {
    const EVENT_TYPE: EventType = EventType::from_str("saga.tell_mock.ReservationRequested");
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
struct Reserved {
    sku: String,
}

impl Event for Reserved {
    const EVENT_TYPE: EventType = EventType::from_str("saga.tell_mock.Reserved");
}

#[derive(Default)]
struct Inventory;

impl Aggregate for Inventory {
    type Event = Reserved;

    fn apply(&mut self, _event: Reserved) {}
}

/// `Reserve` derives `Clone` because the retry executor re-`tell`s with a
/// cloned copy on every attempt.  `Serialize + Deserialize` is required
/// because `SagaEffect::tell` serializes the command as crash-restart payload.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Reserve {
    sku: String,
}

#[async_trait]
impl Decider<Reserve> for Inventory {
    type Rejection = std::convert::Infallible;

    async fn decide(
        &self,
        cmd: Reserve,
        _ctx: &mut Context,
    ) -> Result<Effect<Reserved>, Self::Rejection> {
        Ok(Effect::persist(Reserved { sku: cmd.sku }))
    }
}

struct ReservationSaga {
    inventory: MockAggregateProxy<Inventory>,
    done_notify: Arc<Notify>,
}

#[async_trait]
impl Saga for ReservationSaga {
    type SubscribedEvent = OrderPlaced;
    type Event = ReservationRequested;
    type State = ();
    type Error = std::convert::Infallible;

    fn apply(&mut self, _event: Self::Event) {}

    async fn handle(
        &mut self,
        event: Self::SubscribedEvent,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        let effect = SagaEffect::tell(
            self.inventory.clone(),
            Reserve {
                sku: event.sku.clone(),
            },
        );
        self.done_notify.notify_one();
        Ok(SagaEffect::persist(ReservationRequested { sku: event.sku }).combine(effect))
    }
}

async fn append_order_placed(
    store: &Arc<dyn EventStore>,
    agg_id: &AggregateId,
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
            agg_id.as_str(),
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

#[test]
fn saga_effect_tell_constructor_returns_persist_with_single_tell_intent() {
    // Given a mock target and a typed command
    let mock = MockAggregateProxy::<Inventory>::new();

    // When the saga builds a Tell via the new Builder API
    let effect: SagaEffect<ReservationRequested> = SagaEffect::tell(
        mock,
        Reserve {
            sku: "SKU-tell-construct".into(),
        },
    );

    // Then it is a Persist branch carrying exactly one tell intent and nothing else
    assert_eq!(
        shape_of(&effect),
        Shape::Persist {
            events: vec![],
            tells: 1,
            schedules: vec![],
        },
        "SagaEffect::tell(mock, cmd) must construct a Persist {{ events: [], tells: [1], schedules: [] }} \
         branch — the obsolete top-level Tell variant must not be reintroduced"
    );
}

#[tokio::test]
async fn saga_tell_to_mock_dispatches_command_observable_via_drain_captured() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let order_id = AggregateId::new("saga-tell-mock-order");
    append_order_placed(&upstream_store, &order_id, 1, "SKU-MOCK-1").await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());

    let saga_id = SagaId::new("saga-tell-mock-1");
    let done = Arc::new(Notify::new());

    let mock = MockAggregateProxy::<Inventory>::new();
    let mock_for_saga = mock.clone();
    let done_for_saga = Arc::clone(&done);

    let routed = saga_id.clone();
    let route_fn = move |_event: &OrderPlaced| -> Option<SagaId> { Some(routed.clone()) };

    let _saga_proxy =
        SagaProps::<ReservationSaga>::new(saga_id.clone(), saga_store, move || ReservationSaga {
            inventory: mock_for_saga.clone(),
            done_notify: Arc::clone(&done_for_saga),
        })
        .with_codec(system.codec::<ReservationRequested>())
        .with_subscription(
            Arc::clone(&upstream_store),
            system.codec::<OrderPlaced>(),
            SequenceCursor::Stream {
                key: order_id.as_str().to_owned(),
                after: 0,
            },
            route_fn,
        )
        .spawn(system.process_system())
        .await;

    tokio::time::timeout(Duration::from_secs(3), done.notified())
        .await
        .expect("saga handle() must run within 3 seconds");

    // The Tell side effect dispatches via the outbox executor child process —
    // give it a brief window to land in the mock buffer before draining.
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    let captured = loop {
        let drained = mock.drain_captured::<Reserve>();
        if !drained.is_empty() {
            break drained;
        }
        if std::time::Instant::now() >= deadline {
            panic!("MockAggregateProxy never received the Reserve command within 3 seconds");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };

    assert_eq!(
        captured.len(),
        1,
        "the saga must dispatch exactly one Reserve to the mock for one upstream event \
         (a single-attempt success must not double-dispatch even though the executor performs retries)"
    );
    assert_eq!(
        captured[0].sku, "SKU-MOCK-1",
        "the mock-captured Reserve must carry the sku from the upstream OrderPlaced event"
    );
}
