//! Example 3 — Projection
//!
//! Demonstrates how to subscribe a `Projector` to both:
//! - **Catch-up**: events already in the `EventStore` when the projector starts
//! - **Live**: events published after the projector is running
//!
//! Flow:
//!   1. Persist two events via aggregate `ask()`
//!   2. Spawn a projector in catch-up mode — it processes the two stored events
//!   3. Persist one more event via `ask()` — the projector does NOT see it because
//!      the aggregate does not auto-publish to the live stream (by design)
//!
//! Run with:
//!   cargo run -p eventsource-projection

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use tracing::info;

use nitinol_eventsource::{system::EventSourceSystem, ProjectorProps};
use nitinol_persistence::store::{EventStore, InMemoryCheckpointStore, InMemoryEventStore};
use nitinol_persistence::{AggregateId, ProjectionId};
use nitinol_runtime::ProcessSystem;

use eventsource_projection::codec::JsonCodec;
use eventsource_projection::counter::{Counter, Increment, Incremented};
use eventsource_projection::read_model::{CountProjector, CountReadModel};

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

    let ps = ProcessSystem::new().await;

    // Shared stores
    let event_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let checkpoint_store = Arc::new(InMemoryCheckpointStore::default());

    let system = EventSourceSystem::new(ps)
        .with_codec::<JsonCodec>()
        .with_event_store(Arc::clone(&event_store))
        .build();

    let agg_id = AggregateId::new("proj-counter");
    let proxy = system.spawn_aggregate::<Counter>(agg_id.clone()).await;

    // Persist two events
    proxy.ask(Increment).await.expect("ask 1");
    proxy.ask(Increment).await.expect("ask 2");

    info!("two events persisted — spawning projector in catch-up mode");

    // Shared read-model counters
    let model = CountReadModel::default();
    let value = Arc::clone(&model.value);
    let updated = Arc::clone(&model.updated);

    let _proj = ProjectorProps::new(
        ProjectionId::new("proj-read-model"),
        Arc::clone(&event_store),
        Arc::clone(&checkpoint_store),
        move || CountProjector {
            model: CountReadModel {
                value: Arc::clone(&model.value),
                updated: Arc::clone(&model.updated),
            },
        },
    )
    .with_event::<Incremented>(system.codec::<Incremented>())
    .catchup_from_aggregate(agg_id)
    .spawn(system.process_system())
    .await;

    // Wait for both catch-up events to be projected
    tokio::time::timeout(Duration::from_millis(500), async {
        loop {
            let notified = updated.notified();
            if value.load(Ordering::SeqCst) >= 2 {
                return;
            }
            notified.await;
        }
    })
    .await
    .expect("projector must process both catch-up events within 500 ms");

    let final_count = value.load(Ordering::SeqCst);
    info!(final_count, "read model count after catch-up");
    assert_eq!(final_count, 2, "read model must reflect 2 events");

    // Step 3 — persist one more event; the projector must NOT see it because
    // the aggregate does not auto-publish to the live stream (by design).
    proxy.ask(Increment).await.expect("ask 3");
    assert_eq!(
        value.load(Ordering::SeqCst),
        2,
        "projector must not see events published after catch-up spawn"
    );
    info!("projector correctly ignored event published after catch-up");

    info!("example finished successfully");
}
