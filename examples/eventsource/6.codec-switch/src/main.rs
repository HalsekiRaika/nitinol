//! Example 6 — Codec Switch
//!
//! Demonstrates how to swap the event codec by changing a single type parameter.
//! Three codec variants are shown:
//!
//! 1. `JsonCodec`        — serde_json, human-readable JSON bytes
//! 2. `PassThrough`      — no-op codec for unit-struct events
//! 3. `UserDefinedCodec` — custom format skeleton
//!
//! The aggregate logic (`Counter`, `Increment`, `GetCount`) is identical in all
//! three cases.  Only the `EventSourceSystem::with_codec::<C>()` call changes.
//!
//! Run with:
//!   cargo run -p eventsource-codec-switch

use std::sync::Arc;

use tracing::info;

use nitinol_eventsource::{AggregateProps, EventPersistor};
use nitinol_persistence::store::InMemoryEventStore;
use nitinol_persistence::AggregateId;
use nitinol_runtime::ProcessSystem;

use eventsource_codec_switch::codecs::{JsonCodec, PassThrough, UserDefinedCodec};
use eventsource_codec_switch::counter::{Counter, GetCount, Increment};

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

    // --- JSON codec ---
    {
        let event_ref =
            EventPersistor::spawn(&system, Arc::new(InMemoryEventStore::default())).await;
        let proxy = AggregateProps::<Counter>::new(AggregateId::new("codec-json"), event_ref)
            .with_codec(Arc::new(JsonCodec))
            .spawn(&system)
            .await;
        proxy.ask(Increment).await.expect("ask failed");
        let count = proxy.exec(GetCount).await.expect("exec failed");
        info!(codec = "JsonCodec", count);
        assert_eq!(count, 1);
    }

    // --- PassThrough codec ---
    {
        let event_ref =
            EventPersistor::spawn(&system, Arc::new(InMemoryEventStore::default())).await;
        let proxy = AggregateProps::<Counter>::new(AggregateId::new("codec-pass"), event_ref)
            .with_codec(Arc::new(PassThrough))
            .spawn(&system)
            .await;
        proxy.ask(Increment).await.expect("ask failed");
        let count = proxy.exec(GetCount).await.expect("exec failed");
        info!(codec = "PassThrough", count);
        assert_eq!(count, 1);
    }

    // --- User-defined codec ---
    {
        let event_ref =
            EventPersistor::spawn(&system, Arc::new(InMemoryEventStore::default())).await;
        let proxy = AggregateProps::<Counter>::new(AggregateId::new("codec-user"), event_ref)
            .with_codec(Arc::new(UserDefinedCodec))
            .spawn(&system)
            .await;
        proxy.ask(Increment).await.expect("ask failed");
        let count = proxy.exec(GetCount).await.expect("exec failed");
        info!(codec = "UserDefinedCodec", count);
        assert_eq!(count, 1);
    }

    info!("all three codecs work correctly — example finished successfully");
}
