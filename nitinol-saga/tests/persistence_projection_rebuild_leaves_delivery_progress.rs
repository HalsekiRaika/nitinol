//! Rebuilding a projection does not reach the delivery progress beside it.
//!
//! ```gherkin
//!   Scenario: a projection rebuilt over a store that shares one checkpoint handle
//!     Given a store whose two checkpoint slots were named with one handle
//!     And a projection position and a delivery position recorded in it
//!     When the projection slot is named with a handle of its own and the
//!         projector is started again
//!     Then the projector applies every record from the beginning
//!     And the delivery position is where it was
//!     And a host restarted over the rebuilt store delivers nothing again
//! ```
//!
//! Sharing one store between the two slots is the harder half of the contract
//! and the shape a single-table backend is deployed in, so it is the one this
//! clause stages: if a rebuild reached the delivery progress anywhere, it would
//! be here.
//!
//! What "rebuilt" means is the slot, not a store operation. There is no delete
//! on a checkpoint store and none is added: a projection is rebuilt by naming
//! the store's projection slot with a handle that holds nothing, which is what
//! has the projector resume from no position and apply everything again. The
//! delivery slot is named with the handle it already had, so the two halves of
//! the change are visible side by side — one slot replaced, the other left
//! alone.
//!
//! The counter is read per incarnation rather than cumulatively: each
//! incarnation builds its own projector with a counter of its own, so "applied
//! everything again" is a count on the second one rather than a difference
//! between two readings of the first.

#[path = "common/delivery.rs"]
mod delivery;
use delivery::{
    append_decided, delivery_id, operations_of, partition_count, scope_of, settle, stream_rows,
    wait_until_operations, wait_until_stored, Decided, DeliverySaga, Ledger,
    RecordingCheckpointStore, TEST_POLL_INTERVAL,
};

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::Notify;

use nitinol_eventsource::system::EventSourceSystem;
use nitinol_eventsource::{ProjectionContext, Projector};
use nitinol_persistence::provider::{CustomStore, PersistenceProvider};
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::{AggregateId, ProjectionId};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{SagaDeliveryExt, SagaId};

/// How long a clause waits for a projector to apply what it is catching up on.
const PROJECTION_TIMEOUT: Duration = Duration::from_secs(10);

/// A projection whose whole state is how many decisions it has applied.
///
/// What is being observed is where the projector resumed from, so the count is
/// the projection: a rebuilt one starts at nothing and reaches the whole
/// journal again.
struct CountingProjection {
    applied: Arc<AtomicUsize>,
    notify: Arc<Notify>,
}

#[async_trait]
impl Projector<Decided> for CountingProjection {
    type Error = std::convert::Infallible;

    async fn project(
        &mut self,
        _event: Decided,
        _ctx: &mut ProjectionContext<'_, ()>,
    ) -> Result<(), Self::Error> {
        self.applied.fetch_add(1, Ordering::SeqCst);
        self.notify.notify_one();
        Ok(())
    }
}

/// Wait until `applied` has reached `want`, then answer what it holds.
async fn wait_until_applied(
    applied: &Arc<AtomicUsize>,
    notify: &Arc<Notify>,
    want: usize,
    label: &str,
) -> usize {
    tokio::time::timeout(PROJECTION_TIMEOUT, async {
        loop {
            let notified = notify.notified();
            let seen = applied.load(Ordering::SeqCst);
            if seen >= want {
                return seen;
            }
            notified.await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "{label}: the projection applied {} records, wanted at least {want}",
            applied.load(Ordering::SeqCst)
        )
    })
}

/// The number of decisions seeded into the journal, and therefore the number a
/// rebuilt projection has to apply again.
const DECISIONS: usize = 2;

#[tokio::test]
async fn rebuilding_the_projection_slot_leaves_the_delivery_position_where_it_was() {
    // Given: one event store, and one checkpoint handle named into both
    // checkpoint slots of the store.
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let shared_checkpoints = RecordingCheckpointStore::new();
    let projection = ProjectionId::new("projection-rebuild-read-model");
    let saga_id = SagaId::new("instance-projection-rebuild");
    let target = AggregateId::new("ledger-projection-rebuild");

    append_decided(&store, &saga_id, 1, target.as_str(), "op-1").await;
    let last_decision = append_decided(&store, &saga_id, 2, target.as_str(), "op-2").await;

    let first_system = EventSourceSystem::builder(ProcessSystem::new().await)
        .with_persistence(PersistenceProvider::new(
            CustomStore::builder()
                .eventstore(Arc::clone(&store))
                .checkpoints(Arc::clone(&shared_checkpoints))
                .build(),
        ))
        .build();

    let first_applied = Arc::new(AtomicUsize::new(0));
    let first_notify = Arc::new(Notify::new());
    let applied_for_producer = Arc::clone(&first_applied);
    let notify_for_producer = Arc::clone(&first_notify);
    let _first_projector = first_system
        .projector_props(projection.clone(), move || CountingProjection {
            applied: Arc::clone(&applied_for_producer),
            notify: Arc::clone(&notify_for_producer),
        })
        .with_event::<Decided>()
        .catchup_from_global()
        .spawn()
        .await;

    let _first_host = first_system
        .saga_delivery_props::<DeliverySaga>()
        .with_delivery_id(delivery_id())
        .with_partitions(partition_count(1))
        .with_poll_interval(TEST_POLL_INTERVAL)
        .spawn()
        .await;

    // And: both consumers have recorded where they got to, in the one handle
    // both slots were named with.
    wait_until_applied(
        &first_applied,
        &first_notify,
        DECISIONS,
        "the first projection must apply every decision",
    )
    .await;
    wait_until_stored(
        &shared_checkpoints,
        &projection,
        last_decision,
        "the projection must record its position",
    )
    .await;
    assert_eq!(
        wait_until_stored(
            &shared_checkpoints,
            &scope_of(0),
            last_decision,
            "the delivery host must record its position",
        )
        .await,
        last_decision,
        "the delivery host's position is the decision it last delivered, and \
         its subscription names the decision event type alone, so nothing else \
         in the store moves it"
    );
    let first_ledger = first_system.aggregate_proxy::<Ledger>(target.clone());
    assert_eq!(
        wait_until_operations(&first_ledger, DECISIONS, "both decisions must be delivered").await,
        vec!["op-1".to_owned(), "op-2".to_owned()],
    );

    // When: the projection slot is named with a handle holding nothing and the
    // delivery slot keeps the one it had, and both consumers are started again
    // over the same event store.
    let rebuilt_checkpoints = RecordingCheckpointStore::new();
    let rebuilt_system = EventSourceSystem::builder(ProcessSystem::new().await)
        .with_persistence(PersistenceProvider::new(
            CustomStore::builder()
                .eventstore(Arc::clone(&store))
                .projection_checkpoints(Arc::clone(&rebuilt_checkpoints))
                .delivery_checkpoints(Arc::clone(&shared_checkpoints))
                .build(),
        ))
        .build();

    let rebuilt_applied = Arc::new(AtomicUsize::new(0));
    let rebuilt_notify = Arc::new(Notify::new());
    let applied_for_producer = Arc::clone(&rebuilt_applied);
    let notify_for_producer = Arc::clone(&rebuilt_notify);
    let _rebuilt_projector = rebuilt_system
        .projector_props(projection.clone(), move || CountingProjection {
            applied: Arc::clone(&applied_for_producer),
            notify: Arc::clone(&notify_for_producer),
        })
        .with_event::<Decided>()
        .catchup_from_global()
        .spawn()
        .await;

    let _rebuilt_host = rebuilt_system
        .saga_delivery_props::<DeliverySaga>()
        .with_delivery_id(delivery_id())
        .with_partitions(partition_count(1))
        .with_poll_interval(TEST_POLL_INTERVAL)
        .spawn()
        .await;

    // Then: the rebuilt projection really is a rebuild — it resumed from no
    // position and applied the whole journal again.
    assert_eq!(
        wait_until_applied(
            &rebuilt_applied,
            &rebuilt_notify,
            DECISIONS,
            "the rebuilt projection must apply every decision again"
        )
        .await,
        DECISIONS,
        "a projection slot named with a handle holding nothing has the \
         projector resume from no position, which is what a rebuild is"
    );
    assert!(
        rebuilt_checkpoints
            .stored(&projection)
            .is_some_and(|held| held >= last_decision),
        "and the rebuilt projection records into the slot it was named with, \
         which holds {:?}",
        rebuilt_checkpoints.stored(&projection)
    );

    // And: the delivery position in the shared store is where it was.
    settle().await;
    assert_eq!(
        shared_checkpoints.stored(&scope_of(0)),
        Some(last_decision),
        "rebuilding the projection slot does not reach the delivery slot: the \
         delivery position has to be the one recorded before the rebuild, even \
         though both slots were named with one handle"
    );

    // And: nothing was delivered a second time, which is what an unchanged
    // delivery position means for the receiver.
    assert_eq!(
        operations_of(&rebuilt_system.aggregate_proxy::<Ledger>(target.clone())).await,
        vec!["op-1".to_owned(), "op-2".to_owned()],
        "the receiver carries the two operations it was delivered and no repeat"
    );
    assert_eq!(
        stream_rows(&store, target.as_str()).await.len(),
        DECISIONS,
        "and {DECISIONS} rows stand behind them: a host restarted over the \
         rebuilt store resumes above the delivery position the rebuild left \
         alone"
    );
}
