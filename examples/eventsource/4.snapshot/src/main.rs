//! Example 4 — Snapshot
//!
//! Demonstrates how to use `Snapshotable` to checkpoint an aggregate and
//! recover it on a later activation without replaying every event from the
//! beginning. Nothing below wires a persistor in by hand: `Counter` declares
//! `type Recover = Latest`, and that declaration is the whole of what makes an
//! activation reach for the newest snapshot first.
//!
//! Flow:
//!   1. Persist 5 events through the first system's activation
//!   2. Save a snapshot at sequence = 5 (value = 5) through that system's persistor
//!   3. Add 2 more events (seq = 6, 7) — the same activation, so nothing is replayed
//!   4. Resolve the aggregate on a second system over the same store — a fresh
//!      activation, which recovers from the snapshot (value = 5) and replays
//!      only the 2 events that followed it
//!   5. Final state == 7
//!
//! Run with:
//!   cargo run -p eventsource-snapshot

use bytes::Bytes;
use tracing::info;

use nitinol_eventsource::system::EventSourceSystem;
use nitinol_persistence::provider::PersistenceProvider;
use nitinol_persistence::store::InMemoryStore;
use nitinol_persistence::{AggregateId, PersistedSnapshot};
use nitinol_runtime::ProcessSystem;

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

    // The journal and the snapshot slot both come from this one store, and both
    // systems below are built over it — the snapshot written through the first
    // is the snapshot the second recovers from.
    let store = InMemoryStore::default();

    let writer = EventSourceSystem::builder(ProcessSystem::new().await)
        .with_persistence(PersistenceProvider::new(store.clone()))
        .build();

    let id = AggregateId::new("snap-counter");

    // Step 1: write 5 events.
    let proxy = writer.spawn_aggregate::<Counter>(id.clone()).await;
    for _ in 0..5 {
        proxy.ask(Increment).await.expect("ask must succeed");
    }
    info!("wrote 5 events");

    // Step 2: save a snapshot at seq=5 (value=5). The persistor is the one
    // process the system distributes, so the activation that recovers later
    // reads exactly what is written here.
    writer
        .snapshot_persistor()
        .await
        .save(PersistedSnapshot {
            aggregate_id: id.clone(),
            sequence: 5,
            payload: Bytes::from(5u64.to_be_bytes().to_vec()),
            created_at: jiff::Timestamp::now(),
        })
        .await
        .expect("save snapshot must succeed");
    info!("snapshot saved at seq=5");

    // Step 3: 2 more events (seq=6,7). One system resolves one activation per
    // identity, so this reaches the activation from step 1 and the snapshot
    // plays no part yet.
    proxy.ask(Increment).await.expect("ask 6");
    proxy.ask(Increment).await.expect("ask 7");
    info!("wrote 2 more events (seq=6,7)");

    // Step 4: a second system over the same store. Its registry is its own, so
    // the aggregate is activated afresh here — which is what a restart looks
    // like, and the only way to watch a recovery from inside one process.
    let reader = EventSourceSystem::builder(ProcessSystem::new().await)
        .with_persistence(PersistenceProvider::new(store))
        .build();

    let recovered = reader.spawn_aggregate::<Counter>(id).await;

    let count = recovered.exec(GetCount).await.expect("exec must succeed");
    info!(count, "count after snapshot+delta recovery");
    assert_eq!(count, 7, "snapshot (5) + 2 delta events = 7");

    info!("example finished successfully");
}
