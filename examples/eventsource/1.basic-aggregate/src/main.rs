//! Example 1 — Basic Aggregate
//!
//! Demonstrates the minimal setup needed to run an event-sourced aggregate:
//!
//! 1. Create a `ProcessSystem`
//! 2. Assemble a `PersistenceProvider` over the in-memory stores
//! 3. Build an `EventSourceSystem` on that provider
//! 4. Spawn a `Counter` aggregate and call `ask`, `tell`, `exec`
//!
//! Run with:
//!   cargo run -p eventsource-basic-aggregate

use tracing::info;

use nitinol_eventsource::system::EventSourceSystem;
use nitinol_persistence::provider::PersistenceProvider;
use nitinol_persistence::store::InMemoryStore;
use nitinol_persistence::AggregateId;
use nitinol_runtime::ProcessSystem;

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

    // 2. Assemble the provider every store comes from.
    let persistence = PersistenceProvider::new(InMemoryStore::default());

    // 3. Build the event-source system on it.  Every aggregate spawned below
    //    persists onto the journal that provider supplies — there is no
    //    per-spawn store to name, and so no way to split the world in two.
    //    There is no codec to wire either: `Incremented` names the one that
    //    writes it.
    let system = EventSourceSystem::builder(ps)
        .with_persistence(persistence)
        .build();

    // 4. Spawn the aggregate process.
    let id = AggregateId::new("counter-1");
    let proxy = system.spawn_aggregate::<Counter>(id).await;

    // ask() sends a command and waits for the answer its decision states —
    // here, the counter's new value.
    let value = proxy.ask(Increment).await.expect("ask failed");
    info!(value, "ask(Increment) returned");
    assert_eq!(value, 1, "the first Increment must answer with 1");

    // tell() sends a command without waiting for a reply (fire-and-forget).
    proxy.tell(Increment).await.expect("tell failed");

    // exec() is a read-only query — returns the current state.
    let count = proxy.exec(GetCount).await.expect("exec failed");
    info!(count, "current counter value");

    assert_eq!(count, 2, "two Increments must yield count == 2");
    info!("example finished successfully");
}
