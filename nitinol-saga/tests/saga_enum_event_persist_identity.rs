//! The identity the saga's user-event persist path writes into the column.
//!
//! A saga's own domain events reach its stream through `append_user_events`.
//! Each row carries the event type the declaration states — the one path the
//! registry answers for, and the one whose codec reads the payload back. Which
//! arm of an enum a row holds is carried by the bytes that codec wrote.
//!
//! A persist path that derived a per-value identity instead would write a path
//! no declaration answers for, and every one of those rows would fail its load.

#[path = "common/helpers.rs"]
mod common;
use common::JsonCodec;

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use nitinol_eventsource::{system::EventSourceSystem, Aggregate, Decider, Decision, Event, Never};
use nitinol_persistence::provider::{CustomStore, PersistenceProvider};
use nitinol_persistence::store::{InMemoryEventStore, InMemoryOffsetStore, Journal};
use nitinol_persistence::{AggregateId, EventType, Family, LoadQuery, LoadedEvent, TypeName};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{
    Saga, SagaContext, SagaEffect, SagaId, SagaManagerProps, SagaSubscribe, Subscription,
};

// Upstream aggregate fixtures

#[derive(Clone, Debug, Serialize, Deserialize)]
struct UpstreamTriggered;

impl Event for UpstreamTriggered {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("identity.upstream"),
        TypeName::new("UpstreamTriggered"),
    );
    type Codec = JsonCodec;
}

#[derive(Default)]
struct UpstreamAggregate;

impl Aggregate for UpstreamAggregate {
    type Event = UpstreamTriggered;
    type Recover = Never;
    fn apply(&mut self, _event: UpstreamTriggered) {}
}

struct Trigger;

impl Decider<Trigger> for UpstreamAggregate {
    type Output = ();
    type Rejection = std::convert::Infallible;

    fn decide(&self, _cmd: Trigger) -> Decision<UpstreamTriggered, (), Self::Rejection> {
        Decision::persist(vec![UpstreamTriggered]).output(())
    }
}

// Saga event: an enum whose arms are told apart by the bytes its codec writes

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
enum WorkflowEvent {
    Started,
    Finished,
}

impl Event for WorkflowEvent {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("identity.saga"), TypeName::new("WorkflowEvent"));
    type Codec = JsonCodec;
}

// Saga: emits both arms on each upstream trigger

/// Correlation rule of [`WorkflowSaga`]: one workflow instance watches the
/// single upstream aggregate, so every trigger belongs to it.
const WORKFLOW_SAGA_ID: &str = "identity-workflow-saga";

/// The whole alphabet this saga reacts to: one observed source, no deadline.
#[derive(SagaSubscribe)]
enum Observed {
    Upstream(UpstreamTriggered),
}

struct WorkflowSaga {
    handled: Arc<Notify>,
}

#[async_trait]
impl Saga for WorkflowSaga {
    type Subscribe = Observed;
    type Event = WorkflowEvent;
    type Error = std::convert::Infallible;

    fn correlate(_input: &Self::Subscribe) -> Option<SagaId> {
        Some(SagaId::new(WORKFLOW_SAGA_ID))
    }

    async fn handle(
        &mut self,
        _input: Self::Subscribe,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        self.handled.notify_one();
        Ok(SagaEffect::persist(WorkflowEvent::Started)
            .combine(SagaEffect::persist(WorkflowEvent::Finished)))
    }
}

async fn load_saga_events(store: &Journal, saga_id: &SagaId) -> Vec<LoadedEvent> {
    store
        .load(LoadQuery::by_stream(saga_id))
        .await
        .expect("load saga stream must succeed")
        .try_collect()
        .await
        .expect("collect saga events must succeed")
}

/// Poll the saga's own stream until both domain rows are present, so the
/// assertion is settled by the state it is about rather than by a fixed wait.
async fn wait_for_domain_rows(
    store: &Journal,
    saga_id: &SagaId,
    expected: usize,
) -> Vec<LoadedEvent> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let events = load_saga_events(store, saga_id).await;
        let domain = domain_rows(&events);
        if domain.len() >= expected {
            return events;
        }
        if Instant::now() >= deadline {
            panic!(
                "timed out waiting for {expected} `{}` rows; the saga stream held: {:?}",
                WorkflowEvent::EVENT_TYPE,
                events
                    .iter()
                    .map(|e| e.event_type.to_string())
                    .collect::<Vec<_>>()
            );
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn domain_rows(events: &[LoadedEvent]) -> Vec<&LoadedEvent> {
    events
        .iter()
        .filter(|e| e.event_type == WorkflowEvent::EVENT_TYPE)
        .collect()
}

/// Given a saga whose own event type is an enum, When it persists two different
/// arms in one effect, Then both rows carry the declaration's event type and
/// their payloads decode back to the arms they were written from.
#[tokio::test]
async fn saga_persist_stores_the_declared_event_type_for_every_arm() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps)
        .with_persistence(PersistenceProvider::new(
            CustomStore::builder()
                .eventstore(InMemoryEventStore::default())
                .saga_offsets(InMemoryOffsetStore::default())
                .build(),
        ))
        .build();
    let store = system.persistence().event_store();

    let upstream_id = AggregateId::new("identity-upstream-agg");
    let upstream_proxy = system
        .spawn_aggregate::<UpstreamAggregate>(upstream_id.clone())
        .await;

    let saga_id = SagaId::new(WORKFLOW_SAGA_ID);
    let handled = Arc::new(Notify::new());
    let handled_for_saga = Arc::clone(&handled);

    let _host = SagaManagerProps::<WorkflowSaga>::new(move |_components| WorkflowSaga {
        handled: Arc::clone(&handled_for_saga),
    })
    .observing::<UpstreamTriggered>(Subscription::stream(&upstream_id))
    .spawn(&system)
    .await;

    upstream_proxy
        .ask(Trigger)
        .await
        .expect("ask(Trigger) must succeed");

    tokio::time::timeout(Duration::from_secs(3), handled.notified())
        .await
        .expect("the saga must handle the upstream event within 3 seconds");

    let events = wait_for_domain_rows(&store, &saga_id, 2).await;
    let rows = domain_rows(&events);

    assert_eq!(
        rows.len(),
        2,
        "exactly the two persisted events belong to the saga's own declaration"
    );
    for row in &rows {
        assert_eq!(
            row.event_type,
            WorkflowEvent::EVENT_TYPE,
            "a row of a declared event carries that declaration's canonical path"
        );
    }

    let arms: Vec<WorkflowEvent> = rows
        .iter()
        .map(|row| serde_json::from_slice(&row.payload).expect("a saga row decodes by its codec"))
        .collect();
    assert_eq!(
        arms,
        vec![WorkflowEvent::Started, WorkflowEvent::Finished],
        "the arm a row holds — and the order the two were written in — is carried by the bytes \
         the declaration's codec wrote"
    );
}
