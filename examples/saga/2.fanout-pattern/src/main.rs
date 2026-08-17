//! Runnable demonstration of the pattern this crate exists to show:
//! `DecomposeBatch` -> one fact event -> saga manager delivery -> 32
//! idempotent child creations.
//!
//! The wiring mirrors what `tests/common/helpers.rs` builds for the
//! integration tests; this binary demonstrates it end to end with a
//! self-verifying assertion, in the same shape as
//! `examples/saga/1.basic-saga/src/main.rs`.

use std::sync::Arc;
use std::time::Duration;

use nitinol_eventsource::system::EventSourceSystem;
use nitinol_eventsource::SequenceCursor;
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::AggregateId;
use nitinol_runtime::ProcessSystem;
use nitinol_saga::SagaManagerProps;

use saga_fanout_pattern::batch::{Batch, BatchDecomposed, DecomposeBatch};
use saga_fanout_pattern::codec::JsonCodec;
use saga_fanout_pattern::item::IsCreated;
use saga_fanout_pattern::router::ItemRegistry;
use saga_fanout_pattern::saga::{FanOutSaga, FanOutStarted};

/// Fan-out width the pattern is built around: one fact event names 32 children.
const ITEM_COUNT: usize = 32;

fn item_ids(batch: &str) -> Vec<String> {
    (0..ITEM_COUNT)
        .map(|index| format!("{batch}-item-{index:02}"))
        .collect()
}

#[tokio::main]
async fn main() {
    let ps = ProcessSystem::new().await;
    let system = Arc::new(EventSourceSystem::new(ps).with_codec::<JsonCodec>().build());

    let batch_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let item_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());

    let batch_id = AggregateId::new("example-batch");
    let ids = item_ids(batch_id.as_str());

    let registry = Arc::new(ItemRegistry::new(
        Arc::clone(&system),
        Arc::clone(&item_store),
    ));

    let batch = system
        .spawn_aggregate::<Batch>(batch_id.clone(), Arc::clone(&batch_store))
        .await;

    let registry_for_producer = Arc::clone(&registry);
    let registry_for_factory = Arc::clone(&registry);

    // The manager, not `spawn_saga`, is what fans a single fact event out to
    // many correlated instances — see `Instance management` in this crate's
    // top-level docs.
    let _manager = SagaManagerProps::<FanOutSaga>::new(saga_store, move || {
        FanOutSaga::new(Arc::clone(&registry_for_producer))
    })
    .with_codec(system.codec::<FanOutStarted>())
    .with_subscription(
        Arc::clone(&batch_store),
        system.codec::<BatchDecomposed>(),
        SequenceCursor::Stream {
            key: batch_id.as_str().to_owned(),
            after: 0,
        },
    )
    .with_crash_restart_factory(move |payload: &[u8]| {
        FanOutSaga::crash_restart_intent(&registry_for_factory, payload)
    })
    .spawn(system.process_system())
    .await;

    batch
        .ask(DecomposeBatch { items: ids.clone() })
        .await
        .expect("ask(DecomposeBatch) must succeed");

    // Poll until every child reports itself created. A child's own stream is
    // the only place its creation becomes observable, so this polls that
    // fact directly rather than sleeping for a guessed duration.
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let created_count = loop {
        let mut count = 0;
        for id in &ids {
            let proxy = registry.resolve(id).await;
            if proxy
                .exec(IsCreated)
                .await
                .expect("exec(IsCreated) must succeed")
            {
                count += 1;
            }
        }
        if count == ITEM_COUNT {
            break count;
        }
        if std::time::Instant::now() >= deadline {
            panic!("fan-out must create {ITEM_COUNT} children within 20 seconds (had {count})");
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    };

    assert_eq!(
        created_count, ITEM_COUNT,
        "every child named by the fact event must have been created"
    );

    println!("fan-out example finished: {created_count} children created from one fact event");
}
