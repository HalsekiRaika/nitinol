//! Which identifier a host's recorded progress is filed under.
//!
//! ```gherkin
//!   Scenario: the wiring names the identifier
//!     Given a delivery host wired with an identifier
//!     When it records progress
//!     Then the recorded key is that identifier's
//!
//!   Scenario: the wiring names none
//!     Given a delivery host wired without an identifier
//!     Then the identifier is derived from the event type the saga declares
//! ```
//!
//! The derived identifier is a default and not a rule: two hosts over one saga
//! type — a second deployment, a migration running beside the first — need to
//! be able to keep separate progress, and that is a deployment's choice to
//! make at the wiring.
//!
//! What it is derived *from* matters as much. The key is persisted, so the
//! material has to be something the type declares and keeps across builds. The
//! declared event type is exactly that, and there is one of it per saga type,
//! so the default names the subscription it belongs to.

#[path = "common/delivery.rs"]
mod delivery;
use delivery::{
    append_decided, delivery_persistence, in_memory_system, partition_count, wait_until_stored,
    Decided, DeliverySaga, RecordingOffsetStore, TEST_POLL_INTERVAL,
};

use nitinol_eventsource::Event;
use nitinol_persistence::store::InMemoryEventStore;
use nitinol_persistence::AggregateId;
use nitinol_saga::{SagaDeliveryId, SagaDeliveryProps, SagaId};

#[tokio::test]
async fn the_wired_identifier_is_the_one_progress_is_filed_under() {
    // Given
    let (system, _aggregates) = in_memory_system().await;
    let journal = InMemoryEventStore::default();
    let offsets = RecordingOffsetStore::new();
    let saga_id = SagaId::new("instance-1");
    let target = AggregateId::new("ledger-explicit-id");
    let position = append_decided(&journal, &saga_id, 1, target.as_str(), "op-1").await;

    let named = SagaDeliveryId::new("orders-delivery");

    // When
    let host = SagaDeliveryProps::<DeliverySaga, RecordingOffsetStore>::new(&delivery_persistence(
        &journal, &offsets,
    ))
    .with_delivery_id(named.clone())
    .with_partitions(partition_count(1))
    .with_poll_interval(TEST_POLL_INTERVAL)
    .spawn(&system)
    .await;

    // Then
    assert_eq!(
        host.delivery_id(),
        &named,
        "the host carries the identifier it was wired with"
    );

    let scope = named.offset_scope(0);
    assert_eq!(
        wait_until_stored(
            &offsets,
            &scope,
            position,
            "progress must be filed under the wired identifier"
        )
        .await,
        position,
    );
    assert_eq!(
        offsets.keys(),
        vec![scope.as_str().to_owned()],
        "and under nothing else: a derived identifier mixed in beside it would \
         leave the host reading a key the wiring never named"
    );
}

#[tokio::test]
async fn an_unwired_identifier_is_derived_from_the_declared_event_type() {
    // Given
    let (system, _aggregates) = in_memory_system().await;
    let journal = InMemoryEventStore::default();
    let offsets = RecordingOffsetStore::new();
    let saga_id = SagaId::new("instance-1");
    let target = AggregateId::new("ledger-default-id");
    let position = append_decided(&journal, &saga_id, 1, target.as_str(), "op-1").await;

    // When
    let host = SagaDeliveryProps::<DeliverySaga, RecordingOffsetStore>::new(&delivery_persistence(
        &journal, &offsets,
    ))
    .with_partitions(partition_count(1))
    .with_poll_interval(TEST_POLL_INTERVAL)
    .spawn(&system)
    .await;

    // Then
    let declared = <Decided as Event>::EVENT_TYPE.to_path().to_string();
    assert_eq!(
        host.delivery_id().as_str(),
        declared,
        "the default is the canonical path of the event type the saga declares, \
         which is a name the type itself fixes rather than one a build or a \
         toolchain could change under a persisted key"
    );

    let scope = host.delivery_id().offset_scope(0);
    assert_eq!(
        wait_until_stored(
            &offsets,
            &scope,
            position,
            "progress must be filed under the derived identifier"
        )
        .await,
        position,
    );
}
