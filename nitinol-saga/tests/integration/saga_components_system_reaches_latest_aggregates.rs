//! The system a saga is handed reaches `Recover = Latest` aggregates too, and
//! where the deployment holds no snapshot slot it says so permanently.
//!
//! ```gherkin
//!   Scenario: the deployment holds a snapshot slot
//!     Given a saga hosted on a system over a store holding a snapshot slot
//!     When it asks a `Latest` aggregate through `SagaComponents::system()`
//!     Then the command is accepted
//!
//!   Scenario: the deployment holds no snapshot slot
//!     Given a saga hosted on a system over a store holding no snapshot slot
//!     When it asks a `Latest` aggregate through `SagaComponents::system()`
//!     Then the ask fails with an activation failure
//!     And that failure is classified as permanent
//! ```
//!
//! The handle is the one an instance's producer is given, captured as it is
//! given. Nothing in this file builds a contingent system any other way. A
//! deployment without the slot is a wiring mistake that the typed system turns
//! into a compile error. Through this handle it has to come back as a verdict a
//! saga will not retry.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::Bytes;
use tokio::sync::Notify;

use super::delivery;
use delivery::{
    delivery_system, Decided, Observed, Record, RecordingOffsetStore, Shelf, Triggered,
    FIXTURE_LOCK, PROBE_TIMEOUT, SHELF_SUFFIX, TRIGGER_KEY,
};

use nitinol_eventsource::system::{ContingentSystem, EventSourceSystem};
use nitinol_eventsource::{AskError, Event, Retryability};
use nitinol_persistence::provider::{PersistenceProvider, SagaOffsetSlot};
use nitinol_persistence::store::{InMemoryEventStore, InMemoryStore, Journal};
use nitinol_persistence::{AggregateId, AppendingEvent};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{
    Saga, SagaComponents, SagaContext, SagaEffect, SagaId, SagaSystemExt, Subscription,
};

/// The handle an instance's producer was given, once it has been.
#[derive(Default)]
struct Captured {
    handle: Mutex<Option<ContingentSystem>>,
    notify: Notify,
}

impl Captured {
    /// Wait until a producer has been handed a system, then answer it.
    async fn handle(&self) -> ContingentSystem {
        tokio::time::timeout(PROBE_TIMEOUT, async {
            loop {
                let notified = self.notify.notified();
                if let Some(handle) = self.handle.lock().expect(FIXTURE_LOCK).clone() {
                    return handle;
                }
                notified.await;
            }
        })
        .await
        .expect("the host must build an instance for the observed trigger")
    }
}

/// A saga whose only use is to hand its components' system to the test.
struct CapturingSaga;

#[async_trait]
impl Saga for CapturingSaga {
    type Subscribe = Observed;
    type Event = Decided;
    type Error = std::convert::Infallible;

    fn correlate(_input: &Self::Subscribe) -> Option<SagaId> {
        Some(SagaId::new("components-latest-capture"))
    }

    async fn handle(
        &mut self,
        _input: Self::Subscribe,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        Ok(SagaEffect::None)
    }
}

fn producer(captured: &Arc<Captured>) -> impl Fn(SagaComponents<CapturingSaga>) -> CapturingSaga {
    let captured = Arc::clone(captured);
    move |components| {
        *captured.handle.lock().expect(FIXTURE_LOCK) = Some(components.system().clone());
        captured.notify.notify_one();
        CapturingSaga
    }
}

/// Host [`CapturingSaga`] on `system`, trigger one instance, and answer the
/// system its producer was handed.
async fn hosted_handle<St: SagaOffsetSlot>(system: &EventSourceSystem<St>) -> ContingentSystem {
    let captured = Arc::new(Captured::default());
    let _host = system
        .host::<CapturingSaga>(producer(&captured))
        .observing::<Triggered>(Subscription::stream(TRIGGER_KEY))
        .spawn()
        .await;
    append_trigger(&system.persistence().event_store()).await;
    captured.handle().await
}

/// Append the one row that has the host build an instance.
async fn append_trigger(journal: &Journal) {
    journal
        .append(
            TRIGGER_KEY,
            vec![AppendingEvent {
                sequence: 1,
                event_type: Triggered::EVENT_TYPE,
                payload: Bytes::from(
                    serde_json::to_vec(&Triggered).expect("a fixture event encodes"),
                ),
                occurred_at: jiff::Timestamp::now(),
            }],
        )
        .await
        .expect("appending a fixture trigger succeeds");
}

fn a_record() -> Record {
    Record {
        operation: "op-through-the-components".to_owned(),
    }
}

#[tokio::test]
async fn the_components_system_asks_a_latest_aggregate_on_a_deployment_holding_a_slot() {
    // Given
    let system = EventSourceSystem::builder(ProcessSystem::new().await)
        .with_persistence(PersistenceProvider::new(InMemoryStore::default()))
        .build();
    let handle = hosted_handle(&system).await;
    let target = AggregateId::new(format!("components-latest{SHELF_SUFFIX}"));

    // When / Then
    handle
        .aggregate_proxy::<Shelf>(target)
        .ask(a_record())
        .await
        .expect(
            "a `Latest` aggregate is reachable through the system a saga is \
             handed when the deployment holds a snapshot slot",
        );
}

#[tokio::test]
async fn the_components_system_refuses_a_latest_aggregate_permanently_without_a_slot() {
    // Given: a deployment holding a journal and saga offsets, and no snapshot
    // slot
    let offsets = RecordingOffsetStore::new();
    let system = delivery_system(&InMemoryEventStore::default(), &offsets).await;
    let handle = hosted_handle(&system).await;
    let target = AggregateId::new(format!("components-no-slot{SHELF_SUFFIX}"));

    // When
    let err = handle
        .aggregate_proxy::<Shelf>(target)
        .ask(a_record())
        .await
        .expect_err(
            "a `Latest` aggregate cannot be activated on a deployment without a \
             snapshot slot",
        );

    // Then
    assert!(
        matches!(err, AskError::Activation(_)),
        "the failure must say the aggregate could not be activated, not that a \
         send failed; got {err:?}"
    );
    assert_eq!(
        err.retryability(),
        Retryability::Permanent,
        "the deployment will not grow a snapshot slot, so a saga must not be \
         told to retry"
    );
}
