//! Example 1 — Basic Aggregate
//!
//! Demonstrates the minimal setup needed to run an event-sourced aggregate:
//!
//! 1. Create a `ProcessSystem`
//! 2. Create an `InMemoryEventStore` and wrap it in `Arc<dyn EventStore>`
//! 3. Build an `EventSourceSystem` with a codec and that store as its default
//! 4. Spawn a `Counter` aggregate and call `ask`, `tell`, `exec`
//!
//! Run with:
//!   cargo run -p eventsource-basic-aggregate

use std::sync::Arc;

use tracing::info;

use nitinol_eventsource::system::EventSourceSystem;
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::AggregateId;
use nitinol_runtime::ProcessSystem;

use eventsource_basic_aggregate::codec::JsonCodec;
use eventsource_basic_aggregate::counter::{GetCount, Increment};

/// Aggregate type alias for brevity.
type Counter = eventsource_basic_aggregate::counter::Counter;

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

    // 1. Create the runtime.
    let ps = ProcessSystem::new().await;

    // 2. Create an in-memory event store.
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());

    // 3. Wire codec and default store into the event-source system.  Every
    //    aggregate spawned below persists onto that store unless it names
    //    another one.
    let system = EventSourceSystem::new(ps)
        .with_codec::<JsonCodec>()
        .with_event_store(store)
        .build();

    // 4. Spawn the aggregate process.
    let id = AggregateId::new("counter-1");
    let proxy = system.spawn_aggregate::<Counter>(id).await;

    // ask() sends a command and waits for the persisted events.
    let events = proxy.ask(Increment).await.expect("ask failed");
    info!(?events, "ask(Increment) returned");

    // tell() sends a command without waiting for a reply (fire-and-forget).
    proxy.tell(Increment).await.expect("tell failed");

    // exec() is a read-only query — returns the current state.
    let count = proxy.exec(GetCount).await.expect("exec failed");
    info!(count, "current counter value");

    assert_eq!(count, 2, "two Increments must yield count == 2");
    info!("example finished successfully");
}
