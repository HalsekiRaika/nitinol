//! Regression test: saga user-event persist path stores enum event's
//! arm-specific `variant()` in `LoadedEvent.event_type`.
//!
//! Without the fix in `interpreter.rs` (returning `Vec<(EventType, Bytes)>`
//! from `encode_events` instead of `Vec<Bytes>` and using the per-event
//! `variant()` in `append_user_events`), the stored `LoadedEvent.event_type`
//! always had `variant=None` for enum saga events.

#[path = "common/helpers.rs"]
mod common;
use common::JsonCodec;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use nitinol_eventsource::{
    system::EventSourceSystem, Aggregate, Context, Decider, Effect, Event, SequenceCursor,
};
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::{AggregateId, EventType, Family, LoadQuery, TypeName, Variant};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{Saga, SagaContext, SagaEffect, SagaId, SagaProps};

// Upstream aggregate fixtures

#[derive(Clone, Debug, Serialize, Deserialize)]
struct UpstreamTriggered;

impl Event for UpstreamTriggered {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("variant.upstream"),
        TypeName::new("UpstreamTriggered"),
    );
}

#[derive(Default)]
struct UpstreamAggregate;

impl Aggregate for UpstreamAggregate {
    type Event = UpstreamTriggered;
    fn apply(&mut self, _event: UpstreamTriggered) {}
}

struct Trigger;

#[async_trait]
impl Decider<Trigger> for UpstreamAggregate {
    type Rejection = std::convert::Infallible;

    async fn decide(
        &self,
        _cmd: Trigger,
        _ctx: &mut Context,
    ) -> Result<Effect<UpstreamTriggered>, Self::Rejection> {
        Ok(Effect::persist(UpstreamTriggered))
    }
}

// Saga event: an enum with arm-specific variant() override

#[derive(Clone, Debug, Serialize, Deserialize)]
enum WorkflowEvent {
    Started,
}

impl Event for WorkflowEvent {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("variant.saga"), TypeName::new("WorkflowEvent"));

    fn variant(&self) -> EventType {
        let v = match self {
            WorkflowEvent::Started => Variant::new("Started"),
        };
        EventType::with_variant(
            Family::new("variant.saga"),
            TypeName::new("WorkflowEvent"),
            v,
        )
    }
}

// Saga: emits WorkflowEvent::Started on each upstream trigger

/// Correlation rule of [`WorkflowSaga`]: one workflow instance watches the
/// single upstream aggregate, so every trigger belongs to it.
const WORKFLOW_SAGA_ID: &str = "variant-workflow-saga";

struct WorkflowSaga {
    done: Arc<Notify>,
}

#[async_trait]
impl Saga for WorkflowSaga {
    type SubscribedEvent = UpstreamTriggered;
    type Event = WorkflowEvent;
    type ScheduledMessage = ();
    type Error = std::convert::Infallible;

    fn correlate(_event: &Self::SubscribedEvent) -> Option<SagaId> {
        Some(SagaId::new(WORKFLOW_SAGA_ID))
    }

    fn apply(&mut self, _event: Self::Event) {}

    async fn handle(
        &mut self,
        _event: Self::SubscribedEvent,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        self.done.notify_one();
        Ok(SagaEffect::persist(WorkflowEvent::Started))
    }
}

// Helper: collect all events for a saga stream

async fn load_saga_events(
    store: &Arc<dyn EventStore>,
    saga_id: &SagaId,
) -> Vec<nitinol_persistence::LoadedEvent> {
    store
        .load(LoadQuery::by_stream(saga_id))
        .await
        .expect("load saga stream must succeed")
        .try_collect()
        .await
        .expect("collect saga events must succeed")
}

// Regression test

/// When a saga processes an upstream event and persists an enum `WorkflowEvent`
/// with arm-specific `variant()` override, the `LoadedEvent.event_type.variant()`
/// read back from the store must be `Some("Started")`, not `None`.
///
/// Failure mode before the fix: `encode_events` returned `Vec<Bytes>` and
/// `append_user_events` used the type-level `EVENT_TYPE` (variant=None) for
/// all saga events, discarding per-event `variant()` overrides.
#[tokio::test]
async fn saga_persist_stores_enum_event_variant_in_loaded_event() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps)
        .with_codec::<JsonCodec>()
        .build();
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());

    let upstream_id = AggregateId::new("variant-upstream-agg");
    let upstream_proxy = system
        .spawn_aggregate::<UpstreamAggregate>(upstream_id.clone(), Arc::clone(&store))
        .await;

    let saga_id = SagaId::new(WORKFLOW_SAGA_ID);
    let done = Arc::new(Notify::new());
    let done_for_saga = Arc::clone(&done);

    let _saga_proxy =
        SagaProps::<WorkflowSaga>::new(saga_id.clone(), Arc::clone(&store), move || WorkflowSaga {
            done: Arc::clone(&done_for_saga),
        })
        .with_codec(system.codec::<WorkflowEvent>())
        .with_subscription(
            Arc::clone(&store),
            system.codec::<UpstreamTriggered>(),
            SequenceCursor::Stream {
                key: upstream_id.as_str().to_owned(),
                after: 0,
            },
        )
        .spawn(system.process_system())
        .await;

    // Trigger the upstream aggregate to emit one event
    upstream_proxy
        .ask(Trigger)
        .await
        .expect("ask(Trigger) must succeed");

    // Wait until the saga has persisted
    tokio::time::timeout(Duration::from_secs(3), done.notified())
        .await
        .expect("saga must persist WorkflowEvent::Started within 3 seconds");

    // Give the saga a moment to write to the store after notifying
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Load the saga stream and verify the stored event_type carries variant=Some("Started")
    let saga_events = load_saga_events(&store, &saga_id).await;

    let user_events: Vec<_> = saga_events
        .iter()
        .filter(|e| e.event_type.type_key() == WorkflowEvent::EVENT_TYPE.type_key())
        .collect();

    assert_eq!(
        user_events.len(),
        1,
        "exactly one WorkflowEvent must be stored in the saga stream"
    );
    assert_eq!(
        user_events[0].event_type.variant(),
        Some(Variant::new("Started")),
        "saga user-event persist must store enum event's arm variant (Some), not None; \
         regression for interpreter.rs encode_events returning (EventType, Bytes) pairs"
    );
    assert_eq!(
        user_events[0].event_type.type_key(),
        WorkflowEvent::EVENT_TYPE.type_key(),
        "type_key must match EVENT_TYPE so routing dispatch still works"
    );
}
