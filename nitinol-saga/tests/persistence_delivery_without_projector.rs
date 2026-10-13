//! A delivery host wired from a store that supplies no projection slot.
//!
//! ```gherkin
//!   Scenario: a delivery host over a store with no projector wired
//!     Given a store supplying an event store and delivery checkpoints only
//!     When a delivery host is the only thing wired on it
//!     Then it delivers every decision in the journal
//!     And it records its progress in the store's delivery slot
//!
//!   Scenario: the host recovers the progress it recorded
//!     Given a store whose delivery slot already holds a position
//!     When a second incarnation wires a host over the same durable state
//!     Then its first query resumes above the recorded position
//!     And nothing the first incarnation delivered is delivered again
//! ```
//!
//! The configuration is the subject: the assembly names the event store and the
//! delivery slot, and the projection and snapshot slots are left unprovided. A
//! delivery host that needed a projection slot — or a snapshot one — could not
//! be wired on this store at all, so what the first clause fixes is that a
//! deployment with no projector in it is a deployment the supplier can express.
//!
//! The host is wired through the system's own entry point — `deliver`, the
//! delivery-host counterpart of the one saga-hosting verb `host` — and started
//! with nothing re-injected, so what the first clause also fixes is that the
//! delivery-only shape reaches the argument-free spawn the other hosted props
//! have.
//!
//! The journal and the receiving aggregates are tenants of the one event store
//! the supplier distributes, under keys of their own. That is what a single
//! supplier means: `EventStore` is stream-keyed, so one instance holds the
//! saga's own stream beside the streams of the aggregates its decisions are
//! addressed to.
//!
//! # How the second incarnation is observed
//!
//! There is no handle that stops a running host — the partitions are processes
//! of the system they were spawned on — so the first incarnation is still
//! polling while the second runs. The second incarnation is therefore given a
//! recording handle onto the *same* durable state rather than the same handle:
//! the queries it is judged on are its own, and reaching one data region through
//! a handle of its own is what the identity contract says a supplier may do.
//! Nothing is appended to the journal after the first incarnation finishes with
//! it, so a redelivery by either host would show up as a second row on the
//! receiver's stream.

#[path = "common/delivery.rs"]
mod delivery;
use delivery::{
    append_decided, delivery_id, delivery_persistence, operations_of, partition_count, scope_of,
    settle, stream_rows, wait_until_operations, wait_until_stored, DeliverySaga, Ledger,
    RecordingCheckpointStore, RecordingEventStore, PROBE_INTERVAL, PROBE_TIMEOUT,
    TEST_POLL_INTERVAL,
};

use std::sync::Arc;
use std::time::Instant;

use nitinol_eventsource::system::EventSourceSystem;
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::AggregateId;
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{SagaDeliveryExt, SagaId};

/// Wait until the host has polled the journal `want` times, then answer the
/// positions those polls asked for.
///
/// Only the host's own polls are counted. A receiving aggregate replaying its
/// stream loads from this store too, and the two are told apart by what they
/// name: the host's subscription names the decision event type as a prefix, and
/// an aggregate's replay names a single stream.
async fn wait_until_journal_polls(
    store: &Arc<RecordingEventStore>,
    want: usize,
    label: &str,
) -> Vec<Option<u64>> {
    let deadline = Instant::now() + PROBE_TIMEOUT;
    loop {
        let positions: Vec<Option<u64>> = store
            .queries()
            .into_iter()
            .filter(|query| query.event_type_prefix.is_some())
            .map(|query| query.from_global_sequence)
            .collect();
        if positions.len() >= want {
            return positions;
        }
        if Instant::now() >= deadline {
            panic!(
                "{label}: saw {} polls of the journal, wanted at least {want}",
                positions.len()
            );
        }
        tokio::time::sleep(PROBE_INTERVAL).await;
    }
}

#[tokio::test]
async fn a_store_without_a_projection_slot_still_wires_a_delivery_host() {
    // Given: a store that supplies the event store and the delivery slot, and
    // neither a projection slot nor a snapshot one.
    let journal: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let checkpoints = RecordingCheckpointStore::new();

    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps)
        .with_persistence(delivery_persistence(&journal, &checkpoints))
        .build();

    let saga_id = SagaId::new("instance-delivery-only");
    let target = AggregateId::new("ledger-delivery-only");

    append_decided(&journal, &saga_id, 1, target.as_str(), "op-1").await;
    let second = append_decided(&journal, &saga_id, 2, target.as_str(), "op-2").await;

    // When: the host is wired from the system alone — neither the journal, the
    // checkpoint slot, nor the process system is named here.
    let _host = system
        .deliver::<DeliverySaga>()
        .with_delivery_id(delivery_id())
        .with_partitions(partition_count(1))
        .with_poll_interval(TEST_POLL_INTERVAL)
        .spawn()
        .await;

    // Then
    let ledger = system.aggregate_proxy::<Ledger>(target);
    assert_eq!(
        wait_until_operations(&ledger, 2, "both decisions must be delivered").await,
        vec!["op-1".to_owned(), "op-2".to_owned()],
        "a host wired from a store with no projection slot delivers the \
         decisions in the journal that store supplies"
    );
    assert_eq!(
        wait_until_stored(
            &checkpoints,
            &scope_of(0),
            second,
            "progress must reach the last decision"
        )
        .await,
        second,
        "and it records its progress in the store's delivery slot, which is the \
         only checkpoint slot this store has"
    );
}

#[tokio::test]
async fn a_second_incarnation_resumes_above_the_position_the_first_recorded() {
    // Given: the durable state both incarnations share, and one incarnation
    // that has delivered a decision and recorded the position behind it.
    let durable: Arc<InMemoryEventStore> = Arc::new(InMemoryEventStore::default());
    let checkpoints = RecordingCheckpointStore::new();

    let first_journal: Arc<dyn EventStore> = Arc::clone(&durable) as Arc<dyn EventStore>;
    let first_system = EventSourceSystem::builder(ProcessSystem::new().await)
        .with_persistence(delivery_persistence(&first_journal, &checkpoints))
        .build();

    let saga_id = SagaId::new("instance-delivery-restart");
    let target = AggregateId::new("ledger-delivery-restart");
    let delivered = append_decided(&first_journal, &saga_id, 1, target.as_str(), "op-1").await;

    let _first_host = first_system
        .deliver::<DeliverySaga>()
        .with_delivery_id(delivery_id())
        .with_partitions(partition_count(1))
        .with_poll_interval(TEST_POLL_INTERVAL)
        .spawn()
        .await;
    let first_ledger = first_system.aggregate_proxy::<Ledger>(target.clone());
    assert_eq!(
        wait_until_operations(&first_ledger, 1, "the first incarnation must deliver").await,
        vec!["op-1".to_owned()],
    );
    assert_eq!(
        wait_until_stored(
            &checkpoints,
            &scope_of(0),
            delivered,
            "the first incarnation must record where it got to"
        )
        .await,
        delivered,
    );

    // When: a second incarnation is wired over the same durable state, through
    // a handle of its own, and over the same delivery slot.
    let recording = RecordingEventStore::new(Arc::clone(&durable) as Arc<dyn EventStore>);
    let second_journal: Arc<dyn EventStore> = Arc::clone(&recording) as Arc<dyn EventStore>;
    let second_system = EventSourceSystem::builder(ProcessSystem::new().await)
        .with_persistence(delivery_persistence(&second_journal, &checkpoints))
        .build();
    let _second_host = second_system
        .deliver::<DeliverySaga>()
        .with_delivery_id(delivery_id())
        .with_partitions(partition_count(1))
        .with_poll_interval(TEST_POLL_INTERVAL)
        .spawn()
        .await;

    // Then: every poll it makes over the journal resumes above the recorded
    // position, which is the only place that number could have come from.
    let positions = wait_until_journal_polls(
        &recording,
        2,
        "the second incarnation's host must poll the journal",
    )
    .await;
    assert!(
        positions
            .iter()
            .all(|position| *position == Some(delivered + 1)),
        "a host recovering from the store's delivery slot resumes above the \
         position that slot holds; positions asked for were {positions:?}"
    );

    // And: nothing was delivered a second time.
    settle().await;
    assert_eq!(
        operations_of(&second_system.aggregate_proxy::<Ledger>(target.clone())).await,
        vec!["op-1".to_owned()],
        "the receiver carries the one operation the first incarnation \
         delivered, restored from the store's event store"
    );
    assert_eq!(
        stream_rows(&first_journal, target.as_str()).await.len(),
        1,
        "and one row stands behind it: a recovered position is where reading \
         starts, so the decision already accounted for is not delivered again"
    );
}
