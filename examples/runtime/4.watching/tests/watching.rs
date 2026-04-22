//! Integration tests for the watching example.
//!
//! These tests verify:
//! - `ctx.watch(pid)` delivers `Terminated { why: Stopped }` when the watched process stops normally
//! - `ctx.watch(pid)` for an absent pid delivers `Terminated { why: NotFound }` immediately
//! - `ctx.unwatch(pid)` prevents future `Terminated` notifications
//! - The cascade-stop pattern: downstream stops → on_terminated sets flag → handler returns Err
//!   → SupervisionStrategy::Stop fires → watcher stops → next watcher notified
//! - Normal pipeline data flow: Pull → Transform → Collect produces ["HELLO", "WORLD", "WATCHING"]
//! - Producer returns `None` when all items are exhausted

use std::time::Duration;

use nitinol_runtime::{ProcessSystem, Props, TerminatedReason};

use watching::collector::{Collect, Collector, GetCollected};
use watching::producer::{self, Producer, Pull};
use watching::transformer::{self, DetachDownstream, Transform, Transformer};

// ─── watch_receives_terminated_stopped ───────────────────────────────────────

#[tokio::test]
async fn watch_receives_terminated_stopped() {
    // Given: a Transformer watching a live Collector
    let system = ProcessSystem::new().await;
    let collector_proxy = system.spawn(Props::new(|| Collector::new())).await;
    let collector_pid = collector_proxy.pid();
    let transformer_proxy = system
        .spawn(Props::new(move || Transformer::new(collector_pid)))
        .await;
    // on_start runs asynchronously; wait for ctx.watch() to register
    tokio::time::sleep(Duration::from_millis(50)).await;

    // When: the Collector stops normally
    collector_proxy.stop().await.ok();
    // Wait for the Terminated signal to reach on_terminated
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Then: Transformer's on_terminated was called with Stopped
    let reason = transformer_proxy
        .ask(transformer::CheckDownstream)
        .await
        .expect("ask should succeed");
    assert_eq!(reason, Some(TerminatedReason::Stopped));

    transformer_proxy.stop().await.ok();
}

// ─── watch_receives_terminated_not_found ─────────────────────────────────────

#[tokio::test]
async fn watch_receives_terminated_not_found() {
    // Given: a Collector that has already been stopped (dead pid)
    let system = ProcessSystem::new().await;
    let temp = system.spawn(Props::new(|| Collector::new())).await;
    let dead_pid = temp.pid();
    temp.stop().await.ok();
    // Wait for the process to fully deregister from the registry
    tokio::time::sleep(Duration::from_millis(50)).await;

    // When: a Transformer is spawned with the dead pid as downstream
    // on_start calls ctx.watch(dead_pid); since dead_pid is absent the runtime
    // routes a NotFound notification back via DeadLetterProcess immediately
    let transformer_proxy = system
        .spawn(Props::new(move || Transformer::new(dead_pid)))
        .await;
    // Wait for on_start + NotFound delivery to reach on_terminated
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Then: Transformer's on_terminated was called with NotFound
    let reason = transformer_proxy
        .ask(transformer::CheckDownstream)
        .await
        .expect("ask should succeed");
    assert_eq!(reason, Some(TerminatedReason::NotFound));

    transformer_proxy.stop().await.ok();
}

// ─── unwatch_prevents_terminated_notification ────────────────────────────────

#[tokio::test]
async fn unwatch_prevents_terminated_notification() {
    // Given: a Transformer that has registered a watch and then cancelled it
    let system = ProcessSystem::new().await;
    let collector_proxy = system.spawn(Props::new(|| Collector::new())).await;
    let collector_pid = collector_proxy.pid();
    let transformer_proxy = system
        .spawn(Props::new(move || Transformer::new(collector_pid)))
        .await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // DetachDownstream causes Transformer to call ctx.unwatch(collector_pid)
    transformer_proxy
        .tell(DetachDownstream)
        .await
        .expect("tell should succeed");
    // Wait for the Unwatch signal to be processed before stopping Collector
    tokio::time::sleep(Duration::from_millis(50)).await;

    // When: the Collector stops after the watch has been removed
    collector_proxy.stop().await.ok();
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Then: on_terminated was NOT called — CheckDownstream returns None
    let reason = transformer_proxy
        .ask(transformer::CheckDownstream)
        .await
        .expect("ask should succeed");
    assert_eq!(reason, None);

    transformer_proxy.stop().await.ok();
}

// ─── cascade_stop_propagates_through_pipeline ────────────────────────────────

#[tokio::test]
async fn cascade_stop_propagates_through_pipeline() {
    // Given: a full pipeline (Producer →watches→ Transformer →watches→ Collector)
    let system = ProcessSystem::new().await;
    let collector_proxy = system.spawn(Props::new(|| Collector::new())).await;
    let collector_pid = collector_proxy.pid();
    let transformer_proxy = system
        .spawn(Props::new(move || Transformer::new(collector_pid)))
        .await;
    let transformer_pid = transformer_proxy.pid();
    let producer_proxy = system
        .spawn(Props::new(move || {
            Producer::new(vec!["a".into(), "b".into()], transformer_pid)
        }))
        .await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // When: the tail of the pipeline (Collector) stops
    collector_proxy.stop().await.ok();
    // Wait for Transformer's on_terminated to set downstream_alive = false
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Trigger Transformer's error path: handler sees downstream_alive = false,
    // returns Err(DownstreamTerminated), and SupervisionStrategy::Stop fires
    transformer_proxy
        .tell(Transform("cascade-trigger".into()))
        .await
        .ok();
    // Wait for supervision to stop Transformer and Producer's on_terminated to fire
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Then: Producer received Terminated(Stopped) for its downstream (Transformer)
    let reason = producer_proxy
        .ask(producer::CheckDownstream)
        .await
        .expect("ask should succeed");
    assert_eq!(reason, Some(TerminatedReason::Stopped));

    producer_proxy.stop().await.ok();
}

// ─── normal_pipeline_data_flow ───────────────────────────────────────────────

#[tokio::test]
async fn normal_pipeline_data_flow() {
    // Given: a full pipeline with ["hello", "world", "watching"] as source data
    let system = ProcessSystem::new().await;
    let collector_proxy = system.spawn(Props::new(|| Collector::new())).await;
    let collector_pid = collector_proxy.pid();
    let transformer_proxy = system
        .spawn(Props::new(move || Transformer::new(collector_pid)))
        .await;
    let transformer_pid = transformer_proxy.pid();
    let producer_proxy = system
        .spawn(Props::new(move || {
            Producer::new(
                vec!["hello".into(), "world".into(), "watching".into()],
                transformer_pid,
            )
        }))
        .await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // When: each item is pulled, transformed, and collected — mirrors the main.rs loop
    for _ in 0..3 {
        let item = producer_proxy.ask(Pull).await.expect("ask should succeed");
        let text = item.expect("item should be present");
        let transformed = transformer_proxy
            .ask(Transform(text))
            .await
            .expect("ask should succeed");
        collector_proxy
            .tell(Collect(transformed))
            .await
            .expect("tell should succeed");
    }
    // Wait for the last Collect message to be processed
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Then: Collector holds the uppercase versions of all three items in order
    let collected = collector_proxy
        .ask(GetCollected)
        .await
        .expect("ask should succeed");
    assert_eq!(collected, vec!["HELLO", "WORLD", "WATCHING"]);

    producer_proxy.stop().await.ok();
    transformer_proxy.stop().await.ok();
    collector_proxy.stop().await.ok();
}

// ─── producer_returns_none_when_exhausted ────────────────────────────────────

#[tokio::test]
async fn producer_returns_none_when_exhausted() {
    // Given: a Producer with a single item
    let system = ProcessSystem::new().await;
    // Producer watches a downstream pid via on_start; a throwaway Collector serves
    // as a stand-in — the watch/Terminated path is not under test here
    let dummy = system.spawn(Props::new(|| Collector::new())).await;
    let dummy_pid = dummy.pid();
    let producer_proxy = system
        .spawn(Props::new(move || {
            Producer::new(vec!["only-one".into()], dummy_pid)
        }))
        .await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // When: the single item is pulled
    let first = producer_proxy.ask(Pull).await.expect("ask should succeed");
    assert_eq!(first, Some("only-one".to_string()));

    // Then: a second Pull returns None — the queue is exhausted
    let second = producer_proxy.ask(Pull).await.expect("ask should succeed");
    assert_eq!(second, None);

    producer_proxy.stop().await.ok();
    dummy.stop().await.ok();
}
