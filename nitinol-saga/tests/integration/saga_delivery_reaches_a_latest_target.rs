//! A saga delivers to an aggregate that recovers from its newest snapshot.
//!
//! ```gherkin
//!   Scenario: the deployment holds a snapshot slot
//!     Given a system over a store that holds a snapshot slot
//!     And a snapshot of a `Recover = Latest` receiver saved through the
//!         system's snapshot persistor
//!     When a saga hosted on that system decides a command for the receiver
//!     Then the host records the decision as delivered
//!     And the command landed on the state the snapshot recovered
//! ```
//!
//! The host reaches its targets through the system's contingent system. That
//! carries the deployment's snapshot slot, so the receiver is activated
//! from the snapshot its typed system saved. The snapshot holds an operation
//! the receiver's stream never recorded, so an activation that replayed the
//! stream without it would answer with the delivered operation alone.
//!
//! The receiver is not touched through the typed system until the host has
//! recorded the delivery. The two share one aggregate registry, so a typed
//! dispatch beforehand would be the activation the delivery reached, and the
//! clause would no longer be about the contingent system.
//!
//! Whether the activation resolved the very process the typed system's
//! `snapshot_persistor()` hands out is compared by process identifier in
//! `nitinol-eventsource`, where the recovery is visible. Here the observation
//! is that it reads the snapshot that persistor saved.

use std::time::Instant;

use bytes::Bytes;
use futures_util::TryStreamExt;
use jiff::Timestamp;

use super::delivery;
use delivery::{
    append_decided_to, scope_of, spawn_hosted, Operations, Recorded, Shelf, ShelvingSaga,
    PROBE_INTERVAL, PROBE_TIMEOUT, SHELF_SUFFIX,
};

use nitinol_eventsource::system::EventSourceSystem;
use nitinol_eventsource::Event;
use nitinol_persistence::provider::{PersistenceProvider, SagaOffsetSlot, Store};
use nitinol_persistence::store::{InMemoryStore, Journal, OffsetScope, OffsetStore};
use nitinol_persistence::{AggregateId, LoadQuery, PersistedSnapshot};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::SagaId;

/// The operation the saved snapshot holds and the receiver's stream does not.
const FROM_THE_SNAPSHOT: &str = "from-the-snapshot";

/// Wait until `offsets` holds at least `want` for `scope`, then answer it.
async fn wait_until_recorded<O>(offsets: &O, scope: &OffsetScope, want: u64, label: &str) -> u64
where
    O: OffsetStore<Tx = ()>,
{
    let deadline = Instant::now() + PROBE_TIMEOUT;
    loop {
        let held = offsets
            .load(scope)
            .await
            .expect("loading from an in-memory offset store succeeds");
        if let Some(held) = held.filter(|held| *held >= want) {
            return held;
        }
        if Instant::now() >= deadline {
            panic!(
                "{label}: {} holds {held:?}, wanted at least {want}",
                scope.as_str()
            );
        }
        tokio::time::sleep(PROBE_INTERVAL).await;
    }
}

/// The operations recorded on `id`'s stream, in stored order.
async fn recorded_operations(journal: &Journal, id: &AggregateId) -> Vec<String> {
    let loaded: Vec<_> = journal
        .load(LoadQuery::by_stream(id))
        .await
        .expect("loading a stream of an in-memory store succeeds")
        .try_collect()
        .await
        .expect("collecting a stream of an in-memory store succeeds");
    loaded
        .iter()
        .filter(|event| event.event_type == Recorded::EVENT_TYPE)
        .map(|event| {
            serde_json::from_slice::<Recorded>(&event.payload)
                .expect("the receiver's event is encoded with the codec its type names")
                .operation
        })
        .collect()
}

#[tokio::test]
async fn a_decision_for_a_latest_aggregate_lands_on_the_state_its_snapshot_recovered() {
    // Given: a deployment holding a snapshot slot, and a snapshot of the
    // receiver saved through the typed system's persistor
    let store = InMemoryStore::default();
    let system = EventSourceSystem::builder(ProcessSystem::new().await)
        .with_persistence(PersistenceProvider::new(store.clone()))
        .build();
    let target = AggregateId::new(format!("latest-target{SHELF_SUFFIX}"));
    system
        .snapshot_persistor()
        .await
        .save(PersistedSnapshot {
            aggregate_id: target.clone(),
            sequence: 0,
            payload: Bytes::from(
                serde_json::to_vec(&vec![FROM_THE_SNAPSHOT.to_owned()])
                    .expect("a fixture snapshot encodes"),
            ),
            created_at: Timestamp::now(),
        })
        .await
        .expect("saving through the system's snapshot persistor succeeds");

    let saga_id = SagaId::new("instance-1");
    let position = append_decided_to(&store.events(), &saga_id, 1, target.as_str(), "op-1").await;

    // When
    let _host = spawn_hosted::<ShelvingSaga, _>(&system, 1).await;

    // Then: the host recorded the decision as delivered
    wait_until_recorded(
        &store.saga_offsets(),
        &scope_of(0),
        position,
        "a delivery to a `Latest` aggregate on a deployment holding a snapshot \
         slot must succeed and become progress",
    )
    .await;
    assert_eq!(
        recorded_operations(&store.events(), &target).await,
        vec!["op-1".to_owned()],
        "the delivered command was recorded on the receiver's stream"
    );

    // And: it landed on the state the snapshot recovered
    let operations = system
        .aggregate_proxy::<Shelf>(target)
        .exec(Operations)
        .await
        .expect("querying the receiver succeeds");
    assert_eq!(
        operations,
        vec![FROM_THE_SNAPSHOT.to_owned(), "op-1".to_owned()],
        "the activation the delivery reached started from the snapshot the \
         typed system saved; one that replayed the stream alone would hold \
         only the delivered operation"
    );
}
