// Architecture tests: AggregateProps and AggregateProcess must not hold
// Arc<dyn EventStore> or Arc<dyn SnapshotStore> directly.
//
// Phase 2 design constraint (primary source: user statement):
//   "Context/Props に EventStore 等の DB 接続プールを含めず、
//    専用の永続化用プロセスにメッセージングすることで対応する"
//
// Two verification strategies are used:
//
// 1. **Compile-time type check** (functions prefixed `_assert_sig_`)
//    These are non-#[test] functions that merely assign a function pointer.
//    If `AggregateProps::new` ever regresses to accepting Arc<dyn EventStore>,
//    the assignment fails to compile — the test suite refuses to build.
//
// 2. **Runtime integration check** (functions decorated `#[tokio::test]`)
//    These verify that the full spawn lifecycle completes without panicking,
//    confirming that EventPersistorRef / SnapshotPersistorRef are sufficient
//    to drive an AggregateProcess without any direct store reference.

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;

use nitinol_eventsource::{
    Aggregate, Context, Decider, Effect, Event,
    EventCodec, DecodeError, EncodeError,
    EventPersistor, EventPersistorRef,
    SnapshotPersistor, SnapshotPersistorRef,
    Snapshotable, SnapshotCaptureError, SnapshotRestoreError,
    AggregateProps,
};
use nitinol_persistence::store::{InMemoryEventStore, InMemorySnapshotStore};
use nitinol_persistence::{AggregateId, EventType};
use nitinol_runtime::ProcessSystem;

// ---------------------------------------------------------------------------
// Minimal test aggregate (non-snapshotable)
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Dummy;

#[derive(Clone, PartialEq, Debug)]
struct DummyEvent;

impl Event for DummyEvent {
    const EVENT_TYPE: EventType = EventType::from_str("DummyEvent");
}

impl Aggregate for Dummy {
    type Event = DummyEvent;
    fn apply(&mut self, _event: DummyEvent) {}
}

struct AlwaysIncrement;

#[async_trait]
impl Decider<AlwaysIncrement> for Dummy {
    type Rejection = std::convert::Infallible;
    async fn decide(
        &self,
        _cmd: AlwaysIncrement,
        _ctx: &mut Context,
    ) -> Result<Effect<DummyEvent>, Self::Rejection> {
        Ok(Effect::persist(DummyEvent))
    }
}

// ---------------------------------------------------------------------------
// Minimal test aggregate (snapshotable)
// ---------------------------------------------------------------------------

#[derive(Default)]
struct SnapshotDummy;

impl Aggregate for SnapshotDummy {
    type Event = DummyEvent;
    fn apply(&mut self, _event: DummyEvent) {}
}

impl Snapshotable for SnapshotDummy {
    fn restore(_payload: &[u8]) -> Result<Self, SnapshotRestoreError> {
        Ok(Self)
    }
    fn capture(&self) -> Result<Bytes, SnapshotCaptureError> {
        Ok(Bytes::new())
    }
}

// ---------------------------------------------------------------------------
// Minimal codec
// ---------------------------------------------------------------------------

struct PassThroughCodec;

impl EventCodec<DummyEvent> for PassThroughCodec {
    fn encode(&self, _event: &DummyEvent) -> Result<Bytes, EncodeError> {
        Ok(Bytes::new())
    }
    fn decode(&self, _event_type: EventType, _bytes: Bytes) -> Result<DummyEvent, DecodeError> {
        Ok(DummyEvent)
    }
}

// ---------------------------------------------------------------------------
// Compile-time type-level assertions
//
// These are NOT #[test] functions — they never execute at runtime.
// The Rust compiler checks them at build time; a signature mismatch causes
// a compile error, which is the intended signal.
// ---------------------------------------------------------------------------

/// Asserts that AggregateProps::<Dummy>::new has the signature
/// `fn(AggregateId, EventPersistorRef) -> AggregateProps<Dummy>`.
///
/// If Props is ever changed to accept Arc<dyn EventStore> again, this will not compile.
fn _assert_sig_aggregate_props_new_accepts_event_persistor_ref() {
    let _: fn(AggregateId, EventPersistorRef) -> AggregateProps<Dummy> = AggregateProps::new;
}

/// Asserts that AggregateProps::<SnapshotDummy>::with_snapshot_persistor has the signature
/// `fn(AggregateProps<SnapshotDummy>, SnapshotPersistorRef) -> AggregateProps<SnapshotDummy>`.
///
/// If the method is ever changed to accept Arc<dyn SnapshotStore> again, this will not compile.
fn _assert_sig_aggregate_props_with_snapshot_persistor_accepts_ref() {
    let _: fn(AggregateProps<SnapshotDummy>, SnapshotPersistorRef) -> AggregateProps<SnapshotDummy> =
        AggregateProps::with_snapshot_persistor;
}

// ---------------------------------------------------------------------------
// Runtime integration: EventPersistorRef を使ったスポーン成功
// ---------------------------------------------------------------------------

/// AggregateProps::new(id, EventPersistorRef) で問題なくスポーンできる。
/// Arc<dyn EventStore> を直接受け取らないことを型検査で担保しつつ、
/// 実際に起動・コマンド処理が完了することをランタイムレベルで確認する。
#[tokio::test]
async fn aggregate_props_spawns_with_event_persistor_ref() {
    // Given
    let system = ProcessSystem::new().await;
    let store = Arc::new(InMemoryEventStore::default());
    let event_ref = EventPersistor::spawn(&system, store).await;

    // When: spawn via EventPersistorRef (no Arc<dyn EventStore> in Props)
    let proxy = AggregateProps::<Dummy>::new(AggregateId::new("arch-no-store"), event_ref)
        .with_codec(Arc::new(PassThroughCodec))
        .spawn(&system)
        .await;

    // Then: ask succeeds — the process is alive and connected to the persistor
    proxy.ask(AlwaysIncrement).await.expect("ask must succeed without direct store reference");
}

// ---------------------------------------------------------------------------
// Runtime integration: SnapshotPersistorRef を使ったスポーン成功
// ---------------------------------------------------------------------------

/// Snapshotable な集約も SnapshotPersistorRef 経由でスポーンできる。
/// Arc<dyn SnapshotStore> が Props に含まれないことを型で保証しつつ、
/// 実際に起動できることをランタイムレベルで確認する。
#[tokio::test]
async fn aggregate_props_spawns_snapshotable_with_snapshot_persistor_ref() {
    // Given
    let system = ProcessSystem::new().await;
    let event_store = Arc::new(InMemoryEventStore::default());
    let snapshot_store = Arc::new(InMemorySnapshotStore::default());
    let event_ref = EventPersistor::spawn(&system, event_store).await;
    let snapshot_ref = SnapshotPersistor::spawn(&system, snapshot_store).await;

    // When: spawn with both refs (no Arc<dyn SnapshotStore> in Props)
    let proxy =
        AggregateProps::<SnapshotDummy>::new(AggregateId::new("arch-no-snap-store"), event_ref)
            .with_codec(Arc::new(PassThroughCodec))
            .with_snapshot_persistor(snapshot_ref)
            .spawn(&system)
            .await;

    // Then: process spawns without panic
    drop(proxy);
}

// ---------------------------------------------------------------------------
// Runtime integration: 2 つのAggregateProcessが同じEventPersistorRefを共有できる
// ---------------------------------------------------------------------------

/// 単一の EventPersistor を複数の AggregateProcess が共有して使用できる。
/// これはスケーリングパターンの基礎となる設計を確認するテスト。
/// DB Lock への依存がないため、複数プロセスが同一アクターへ
/// ロックレスにメッセージングできることを間接的に確認する。
#[tokio::test]
async fn multiple_aggregate_processes_share_single_event_persistor() {
    // Given: one EventPersistor serves two different aggregate IDs
    let system = ProcessSystem::new().await;
    let store = Arc::new(InMemoryEventStore::default());
    let event_ref = EventPersistor::spawn(&system, store).await;

    let proxy_a = AggregateProps::<Dummy>::new(AggregateId::new("arch-shared-a"), event_ref.clone())
        .with_codec(Arc::new(PassThroughCodec))
        .spawn(&system)
        .await;

    let proxy_b = AggregateProps::<Dummy>::new(AggregateId::new("arch-shared-b"), event_ref.clone())
        .with_codec(Arc::new(PassThroughCodec))
        .spawn(&system)
        .await;

    // When: both processes append events independently
    proxy_a.ask(AlwaysIncrement).await.expect("proxy_a ask must succeed");
    proxy_b.ask(AlwaysIncrement).await.expect("proxy_b ask must succeed");
    proxy_a.ask(AlwaysIncrement).await.expect("proxy_a second ask must succeed");

    // Then: both succeed, confirming lockless shared access to the same persistor
    // (no deadlock, no panic, no contention error)
}
