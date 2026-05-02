//! Example 5 — Aggregate Communication
//!
//! Demonstrates how two aggregates communicate via `Effect::Side` (fire-and-forget).
//!
//! This is the "Send-and-pray" pattern: Aggregate A's `decide()` returns an
//! `Effect::Side(TellTargetEffect)` that calls `proxy_b.tell(Increment)`.
//! No compensation logic is included — if the side effect fails, the caller
//! of `ask()` does NOT see the error.
//!
//! Note: This is intentionally NOT a Saga.  Saga / Process Manager is out of
//! scope for Phase 2.
//!
//! Run with:
//!   cargo run -p eventsource-aggregate-communication

use std::sync::Arc;
use std::time::Duration;

use tracing::info;

use nitinol_eventsource::{system::EventSourceSystem, EventPersistor};
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::AggregateId;
use nitinol_runtime::ProcessSystem;
use tokio::sync::Notify;

use eventsource_aggregate_communication::codec::JsonCodec;
use eventsource_aggregate_communication::counter::{Counter, DelegateToTarget, GetCount};

fn init_tracing() {
    use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(filter)
        .init();
}

async fn spawn_counter(
    system: &EventSourceSystem<JsonCodec>,
    id: &str,
) -> nitinol_eventsource::AggregateProxy<Counter> {
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let event_ref = EventPersistor::spawn(system.process_system(), store).await;
    system
        .spawn_aggregate::<Counter>(AggregateId::new(id), event_ref)
        .await
}

#[tokio::main]
async fn main() {
    init_tracing();

    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();

    let proxy_a = spawn_counter(&system, "comm-a").await;
    let proxy_b = spawn_counter(&system, "comm-b").await;

    let done = Arc::new(Notify::new());

    // A asks to delegate: decide() returns Effect::Side(TellTargetEffect)
    // ask() returns immediately with an empty event list (no persisted events)
    let events = proxy_a
        .ask(DelegateToTarget {
            target: proxy_b.clone(),
            done: Arc::clone(&done),
        })
        .await
        .expect("ask must succeed");

    assert!(events.is_empty(), "DelegateToTarget produces no persisted events");
    info!(?events, "ask(DelegateToTarget) returned");

    // Wait for the side effect to fire
    tokio::time::timeout(Duration::from_millis(300), done.notified())
        .await
        .expect("side effect must fire within 300 ms");

    let count_b = proxy_b.exec(GetCount).await.expect("exec failed");
    info!(count_b, "B counter after receiving delegated Increment");
    assert_eq!(count_b, 1, "B must have received one Increment");

    info!("example finished successfully");
}
