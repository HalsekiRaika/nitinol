//! A fan-out interrupted part-way must finish on replay.
//!
//! The first incarnation runs against an item store that refuses to append to
//! all but a handful of child streams, so the fan-out stops with some children
//! created and the rest missing — the durable state a crash leaves behind.  The
//! second incarnation starts fresh (new `ProcessSystem`, new child processes,
//! a healthy item store) over the same batch and saga stores.  Its manager
//! cursor begins at zero, so the fact event is delivered again, and the pattern
//! has to complete the missing creations without disturbing the ones that
//! already exist.
//!
//! This is the reason the fan-out needs no compensation: the interrupted work
//! is finished by re-running it, because creation is idempotent per child.

#[path = "common/helpers.rs"]
mod common;
use common::{
    created, fingerprints, item_ids, load_stream, spawn_world, wait_for_created,
    PartialFailureItemStore, ITEM_COUNT,
};

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use nitinol_eventsource::Event;
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::AggregateId;

use saga_fanout_pattern::batch::DecomposeBatch;
use saga_fanout_pattern::item::ItemCreated;

/// Budget for a 32-wide fan-out to reach the item store.  A deadline, not a
/// synchronisation device — every wait below polls an observable condition.
const FANOUT_TIMEOUT: Duration = Duration::from_secs(20);

/// How many children the interrupted incarnation is allowed to create.
///
/// Strictly between zero and [`ITEM_COUNT`]: at zero the second run would just
/// be a first run, and at `ITEM_COUNT` there would be nothing left to complete.
const SURVIVING_CHILDREN: usize = 8;

#[tokio::test]
async fn replay_completes_the_children_an_interrupted_fanout_never_created() {
    let batch_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let item_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());

    let batch_id = AggregateId::new("fanout-crash-replay");
    let ids = item_ids(batch_id.as_str());
    let (survivors, missing) = ids.split_at(SURVIVING_CHILDREN);
    let survivors = survivors.to_vec();
    let missing = missing.to_vec();

    // Run 1 — the fan-out is cut off part-way.

    let interrupted_store =
        PartialFailureItemStore::new(Arc::clone(&item_store), survivors.iter().cloned().collect());

    let first_run = spawn_world(
        &batch_id,
        Arc::clone(&batch_store),
        Arc::clone(&interrupted_store) as Arc<dyn EventStore>,
        Arc::clone(&saga_store),
    )
    .await;

    first_run
        .batch
        .ask(DecomposeBatch { items: ids.clone() })
        .await
        .expect("ask(DecomposeBatch) must succeed");

    wait_for_created(&item_store, &survivors, SURVIVING_CHILDREN, FANOUT_TIMEOUT).await;
    let refused = interrupted_store
        .wait_until_refused(ITEM_COUNT - SURVIVING_CHILDREN, FANOUT_TIMEOUT)
        .await;

    assert_eq!(
        refused,
        missing.iter().cloned().collect::<HashSet<String>>(),
        "the interrupted run must have reached — and failed on — exactly the \
         children it was not allowed to create"
    );
    assert_eq!(
        created(&item_store, &ids).await,
        survivors,
        "the interrupted run must leave a genuinely partial fan-out: the allowed \
         children created, the rest absent"
    );

    let survivor_records = fingerprints(&item_store, &survivors).await;

    // Stopping the manager cascade-stops its poller and its saga instances.
    // The child processes belong to the system, not to the manager, so run 2
    // deliberately builds a new system rather than reusing this one — a replay
    // must restore child state from the store, not inherit it from memory.
    first_run
        .manager
        .stop()
        .await
        .expect("stopping the manager must succeed");

    // Run 2 — replay over the same durable state, with a healthy item store.

    // Bound for the rest of the test: this handle owns the second incarnation's
    // `ProcessSystem`, and the assertions below observe the state it produces.
    let _second_run = spawn_world(
        &batch_id,
        Arc::clone(&batch_store),
        Arc::clone(&item_store),
        Arc::clone(&saga_store),
    )
    .await;

    wait_for_created(&item_store, &ids, ITEM_COUNT, FANOUT_TIMEOUT).await;

    let all_records = fingerprints(&item_store, &ids).await;
    for (key, stream) in ids.iter().zip(&all_records) {
        assert_eq!(
            stream.len(),
            1,
            "child {key} must hold exactly one creation record after replay"
        );
        assert_eq!(
            stream[0].0, 1,
            "child {key}'s creation must occupy its genesis sequence"
        );
    }

    assert_eq!(
        &all_records[..SURVIVING_CHILDREN],
        survivor_records.as_slice(),
        "replay must not rewrite the children the interrupted run already \
         created: same stream sequences, same global sequences, same payloads"
    );

    for key in &missing {
        let stream = load_stream(&item_store, key).await;
        assert_eq!(
            stream[0].event_type,
            ItemCreated::EVENT_TYPE,
            "child {key} must have been completed by the replay with its own \
             creation event"
        );
    }

    assert_eq!(
        load_stream(&batch_store, batch_id.as_str()).await.len(),
        1,
        "replaying the fan-out must not add records to the stream that owns the \
         decision"
    );
}
