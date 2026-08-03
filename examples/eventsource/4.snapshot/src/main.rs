//! Example 4 — Snapshot
//!
//! Demonstrates how to use `Snapshotable` to checkpoint an aggregate and
//! restore it on the next spawn without replaying every event from the beginning.
//!
//! Flow:
//!   1. Persist 5 events via process 1
//!   2. Manually save a snapshot at sequence = 5 (value = 5)
//!   3. Add 2 more events via process 2 (seq = 6, 7)
//!   4. Spawn process 3 — restores from the snapshot (value = 5) and applies only 2 delta events
//!   5. Final state == 7
//!
//! Run with:
//!   cargo run -p eventsource-snapshot

use std::sync::Arc;

use bytes::Bytes;
use tracing::info;

use nitinol_eventsource::{AggregateProps, SnapshotPersistor};
use nitinol_persistence::store::{EventStore, InMemoryEventStore, InMemorySnapshotStore};
use nitinol_persistence::{AggregateId, PersistedSnapshot};
use nitinol_runtime::ProcessSystem;

use eventsource_snapshot::codec::{EventCodec, SnapshotCodec};
use eventsource_snapshot::counter::{Counter, GetCount, Increment};

fn init_tracing() {
    use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(filter)
        .init();
}

#[tokio::main]
async fn main() {
    init_tracing();

    let system = ProcessSystem::new().await;
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let snapshot_ref =
        SnapshotPersistor::spawn(&system, Arc::new(InMemorySnapshotStore::default())).await;
    let id = AggregateId::new("snap-counter");

    // Process 1: write 5 events
    {
        let proxy = AggregateProps::<Counter>::new(id.clone(), Arc::clone(&store))
            .with_codec(Arc::new(EventCodec))
            .with_snapshot_persistor(snapshot_ref.clone(), Arc::new(SnapshotCodec))
            .spawn(&system)
            .await;
        for _ in 0..5 {
            proxy.ask(Increment).await.expect("ask must succeed");
        }
        info!("process 1 wrote 5 events");
    }

    // Save a snapshot at seq=5 (value=5)
    snapshot_ref
        .save(PersistedSnapshot {
            aggregate_id: id.clone(),
            sequence: 5,
            payload: Bytes::from(5u64.to_be_bytes().to_vec()),
            created_at: jiff::Timestamp::now(),
        })
        .await
        .expect("save snapshot must succeed");
    info!("snapshot saved at seq=5");

    // Process 2: add 2 more events (seq=6,7)
    {
        let proxy = AggregateProps::<Counter>::new(id.clone(), Arc::clone(&store))
            .with_codec(Arc::new(EventCodec))
            .with_snapshot_persistor(snapshot_ref.clone(), Arc::new(SnapshotCodec))
            .spawn(&system)
            .await;
        proxy.ask(Increment).await.expect("ask 6");
        proxy.ask(Increment).await.expect("ask 7");
        info!("process 2 wrote 2 more events (seq=6,7)");
    }

    // Process 3: restore from snapshot (seq=5) + apply delta events (seq=6,7)
    let proxy3 = AggregateProps::<Counter>::new(id.clone(), Arc::clone(&store))
        .with_codec(Arc::new(EventCodec))
        .with_snapshot_persistor(snapshot_ref.clone(), Arc::new(SnapshotCodec))
        .spawn(&system)
        .await;

    let count = proxy3.exec(GetCount).await.expect("exec must succeed");
    info!(count, "count after snapshot+delta restore");
    assert_eq!(count, 7, "snapshot (5) + 2 delta events = 7");

    info!("example finished successfully");
}
