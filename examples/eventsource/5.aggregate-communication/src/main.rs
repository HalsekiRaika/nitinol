//! Example 5 — Aggregate Communication
//!
//! How one aggregate causes something to happen in another.
//!
//! An aggregate does not reach out.  `Decider::decide` is pure and synchronous:
//! it returns a `Decision` — the facts a command produced and the answer it
//! asks for — and a value cannot tell anybody anything.  What reaches out is a
//! **saga**: it subscribes to the first aggregate's stream, and for each fact it
//! sees, it writes its own record and dispatches a command to the second.
//!
//! # Migrating from `Effect::Side`
//!
//! Earlier versions let `decide` return `Effect::Side(..)`, a fire-and-forget
//! future the activation spawned after the append.  It is gone, and this is
//! what replaces it:
//!
//! | Before | Now |
//! |---|---|
//! | `Ok(Effect::Side(Box::new(TellTargetEffect { target, .. })))` | `SagaEffect::tell(self.target.clone(), Increment)` inside `Saga::handle` |
//! | the command carries the target proxy | the saga holds the target proxy |
//! | the dispatch is lost if the process dies between append and send | the dispatch is an outbox marker in the same atomic append, and is re-issued after a restart |
//! | a duplicate activation performs it twice, undetected | the saga's own stream arbitrates whether it happened at all |
//!
//! The behaviour is not merely relocated: `Effect::Side` was "send and pray",
//! and a saga is the durable version of the same intent.
//!
//! Run with:
//!   cargo run -p eventsource-aggregate-communication

use std::sync::Arc;
use std::time::Duration;

use tracing::info;

use nitinol_eventsource::system::EventSourceSystem;
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::AggregateId;
use nitinol_runtime::ProcessSystem;
use nitinol_saga::SagaDefaultStoreExt;

use eventsource_aggregate_communication::codec::JsonCodec;
use eventsource_aggregate_communication::counter::{Counter, GetCount, Increment};
use eventsource_aggregate_communication::saga::RelaySaga;

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

    // `EventStore` is stream-keyed, so both counters and the saga's own journal
    // are tenants of this one store, each under its own key.
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let system = EventSourceSystem::builder(ps)
        .with_codec::<JsonCodec>()
        .with_event_store(store)
        .build();

    let id_a = AggregateId::new("comm-a");
    let proxy_a = system.spawn_aggregate::<Counter>(id_a.clone()).await;
    let proxy_b = system
        .spawn_aggregate::<Counter>(AggregateId::new("comm-b"))
        .await;

    let target = proxy_b.clone();
    let _saga_proxy = system
        .spawn_saga(RelaySaga::instance_id(), move || RelaySaga {
            target: target.clone(),
        })
        .subscribed_to(system.subscription(&id_a))
        .spawn()
        .await;

    // A decides, and answers with what the command asked for: its new value.
    // Nothing about B is decided here, and nothing about B is awaited here.
    let count_a = proxy_a.ask(Increment).await.expect("ask must succeed");
    assert_eq!(count_a, 1, "A must answer with its own new value");
    info!(count_a, "ask(Increment) on A returned");

    // The relay is asynchronous — A's ask returned the moment A's own fact was
    // durable, which is before the saga had seen it.
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    let count_b = loop {
        let c = proxy_b.exec(GetCount).await.expect("exec must succeed");
        if c >= 1 {
            break c;
        }
        if std::time::Instant::now() >= deadline {
            panic!("the saga must drive an Increment into B within 3 seconds");
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    assert_eq!(count_b, 1, "B must have received exactly one Increment");

    info!(count_b, "B counter after the relayed Increment");
    info!("example finished successfully");
}
