// Integration tests: Tx provider wiring.
//
// When a TxProvider is supplied to ProjectorProps via `with_tx_provider`,
// the framework must:
//
// 1. Call provider.begin() before project() for each event.
// 2. Pass the resulting Tx to ProjectionContext so ctx.tx() returns Some(&mut Tx).
// 3. Call checkpoint_store.save(..., Some(&mut tx)) in ExactlyOnce mode.
// 4. Call provider.commit(tx) after a successful project().
//
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use tokio::sync::Notify;

use nitinol_eventsource::{
    codec::Codec, Event, ProjectionContext, Projector, ProjectorProps, TxProvider,
};
use nitinol_persistence::error::CheckpointError;
use nitinol_persistence::store::{CheckpointStore, EventStore, InMemoryEventStore};
use nitinol_persistence::{AggregateId, AppendingEvent, EventType, Family, TypeName, ProjectionId};
use nitinol_runtime::ProcessSystem;

// ---------------------------------------------------------------------------
// Fixtures: minimal event type
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct TickEvent;

impl Event for TickEvent {
    const EVENT_TYPE: EventType = EventType::new(Family::new(""), TypeName::new("TickEvent"));
}

struct TickCodec;

impl Codec<TickEvent> for TickCodec {
    type Error = std::convert::Infallible;

    fn encode(_: &TickEvent) -> Result<Bytes, Self::Error> {
        Ok(Bytes::new())
    }

    fn decode(_: &[u8]) -> Result<TickEvent, Self::Error> {
        Ok(TickEvent)
    }
}

// ---------------------------------------------------------------------------
// Fixtures: MockTx
// ---------------------------------------------------------------------------

/// A concrete transaction token produced by MockTxProvider.
/// The unique `id` enables identity verification between begin() and commit().
pub struct MockTx {
    pub id: u64,
}

// ---------------------------------------------------------------------------
// Fixtures: MockTxProvider + observable state
// ---------------------------------------------------------------------------

/// Records invocations of begin(), commit(), and rollback() for test assertions.
pub struct MockTxProviderState {
    pub begun: Arc<AtomicU64>,
    pub committed: Arc<AtomicU64>,
    pub rolled_back: Arc<AtomicU64>,
}

pub struct MockTxProvider {
    next_id: Arc<AtomicU64>,
    begun: Arc<AtomicU64>,
    committed: Arc<AtomicU64>,
    rolled_back: Arc<AtomicU64>,
}

impl MockTxProvider {
    fn new() -> (Self, MockTxProviderState) {
        let next_id = Arc::new(AtomicU64::new(1));
        let begun = Arc::new(AtomicU64::new(0));
        let committed = Arc::new(AtomicU64::new(0));
        let rolled_back = Arc::new(AtomicU64::new(0));
        let provider = MockTxProvider {
            next_id: Arc::clone(&next_id),
            begun: Arc::clone(&begun),
            committed: Arc::clone(&committed),
            rolled_back: Arc::clone(&rolled_back),
        };
        let state = MockTxProviderState {
            begun,
            committed,
            rolled_back,
        };
        (provider, state)
    }
}

#[async_trait]
impl TxProvider for MockTxProvider {
    type Tx = MockTx;
    type Error = std::convert::Infallible;

    async fn begin(&self) -> Result<MockTx, Self::Error> {
        self.begun.fetch_add(1, Ordering::SeqCst);
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        Ok(MockTx { id })
    }

    async fn commit(&self, _tx: MockTx) -> Result<(), Self::Error> {
        self.committed.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn rollback(&self, _tx: MockTx) {
        self.rolled_back.fetch_add(1, Ordering::SeqCst);
    }
}

// ---------------------------------------------------------------------------
// Fixtures: MockCheckpointStore
// ---------------------------------------------------------------------------

/// A CheckpointStore with Tx = MockTx.
///
/// Records whether save() was called with Some(tx) (ExactlyOnce+TxProvider path)
/// or None (all other paths).
pub struct MockCheckpointStore {
    state: Mutex<HashMap<ProjectionId, u64>>,
    /// True if save() was ever called with Some(&mut tx).
    saved_with_tx: Arc<AtomicBool>,
}

impl MockCheckpointStore {
    fn new() -> (Arc<Self>, Arc<AtomicBool>) {
        let saved_with_tx = Arc::new(AtomicBool::new(false));
        let store = Arc::new(Self {
            state: Mutex::new(HashMap::new()),
            saved_with_tx: Arc::clone(&saved_with_tx),
        });
        (store, saved_with_tx)
    }
}

#[async_trait]
impl CheckpointStore for MockCheckpointStore {
    type Tx = MockTx;

    async fn load(&self, id: &ProjectionId) -> Result<Option<u64>, CheckpointError> {
        Ok(self.state.lock().unwrap().get(id).copied())
    }

    async fn save(
        &self,
        id: &ProjectionId,
        seq: u64,
        tx: Option<&mut Self::Tx>,
    ) -> Result<(), CheckpointError> {
        if tx.is_some() {
            self.saved_with_tx.store(true, Ordering::SeqCst);
        }
        self.state.lock().unwrap().insert(id.clone(), seq);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Fixtures: TxCapturingProjector
// ---------------------------------------------------------------------------

/// Projector that records whether ctx.tx() returned Some during project().
struct TxCapturingProjector {
    received_tx: Arc<AtomicBool>,
    notify: Arc<Notify>,
}

// ---------------------------------------------------------------------------
// Fixtures: FailingProjector
// ---------------------------------------------------------------------------

/// Concrete error type for use in FailingProjector.
#[derive(Debug)]
struct IntentionalError;

impl std::fmt::Display for IntentionalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "intentional failure for rollback test")
    }
}

impl std::error::Error for IntentionalError {}

/// Projector that always returns Err from project(), triggering rollback.
struct FailingProjector {
    notify: Arc<Notify>,
}

#[async_trait]
impl Projector<TickEvent, MockTx> for FailingProjector {
    type Error = IntentionalError;

    async fn project(
        &mut self,
        _event: TickEvent,
        _ctx: &mut ProjectionContext<'_, MockTx>,
    ) -> Result<(), Self::Error> {
        self.notify.notify_one();
        Err(IntentionalError)
    }
}

#[async_trait]
impl Projector<TickEvent, MockTx> for TxCapturingProjector {
    type Error = std::convert::Infallible;

    async fn project(
        &mut self,
        _event: TickEvent,
        ctx: &mut ProjectionContext<'_, MockTx>,
    ) -> Result<(), Self::Error> {
        // Record whether a transaction was provided by the framework.
        if ctx.tx().is_some() {
            self.received_tx.store(true, Ordering::SeqCst);
        }
        self.notify.notify_one();
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Helper: append one TickEvent to the store
// ---------------------------------------------------------------------------

async fn append_tick(store: &InMemoryEventStore, agg_id: &AggregateId, seq: u64) {
    store
        .append(
            agg_id.as_str(),
            vec![AppendingEvent {
                sequence: seq,
                event_type: EventType::new(Family::new(""), TypeName::new("TickEvent")),
                payload: Bytes::new(),
                occurred_at: jiff::Timestamp::now(),
            }],
        )
        .await
        .expect("append must succeed");
}

// ---------------------------------------------------------------------------
// Helper: wait for a Notify with a timeout
// ---------------------------------------------------------------------------

async fn wait_for(notify: &Arc<Notify>) {
    tokio::time::timeout(Duration::from_millis(500), notify.notified())
        .await
        .expect("timed out waiting for project() to be called");
}

// ---------------------------------------------------------------------------
// Test 1: ctx.tx() returns Some when TxProvider is configured
// ---------------------------------------------------------------------------

/// When TxProvider is configured on ProjectorProps, the framework must call
/// provider.begin() and populate ProjectionContext with the resulting Tx.
/// Inside project(), ctx.tx() must return Some(&mut MockTx).
#[tokio::test]
async fn tx_provider_supplies_tx_to_projection_context() {
    // Given
    let system = ProcessSystem::new().await;
    let store = Arc::new(InMemoryEventStore::default());
    let (mock_cs, _saved_flag) = MockCheckpointStore::new();
    let agg_id = AggregateId::new("tx-ctx-test");

    append_tick(&store, &agg_id, 1).await;

    let received_tx = Arc::new(AtomicBool::new(false));
    let notify = Arc::new(Notify::new());
    let received_tx_c = Arc::clone(&received_tx);
    let notify_c = Arc::clone(&notify);

    let (tp, _tp_state) = MockTxProvider::new();

    // When: spawn projector with TxProvider (ExactlyOnce is forced automatically)
    let _proxy = ProjectorProps::new(
        ProjectionId::new("tx-ctx-test-proj"),
        Arc::clone(&store) as Arc<dyn EventStore>,
        Arc::clone(&mock_cs),
        move || TxCapturingProjector {
            received_tx: Arc::clone(&received_tx_c),
            notify: Arc::clone(&notify_c),
        },
    )
    .with_tx_provider(tp)
    .with_event::<TickEvent>(Arc::new(TickCodec))
    .catchup_from_aggregate(agg_id)
    .spawn(&system)
    .await;

    wait_for(&notify).await;

    // Then: ctx.tx() was Some inside project()
    assert!(
        received_tx.load(Ordering::SeqCst),
        "ctx.tx() must return Some(&mut MockTx) when TxProvider is configured"
    );
}

// ---------------------------------------------------------------------------
// Test 2: ExactlyOnce + TxProvider calls checkpoint save with Some(tx)
// ---------------------------------------------------------------------------

/// In ExactlyOnce mode with a TxProvider, the framework must call
/// checkpoint_store.save(..., Some(&mut tx)) rather than save(..., None).
/// This enables the user's CheckpointStore to atomically save the checkpoint
/// and read-model update within the same transaction.
#[tokio::test]
async fn exactly_once_with_tx_provider_saves_checkpoint_in_same_tx() {
    // Given
    let system = ProcessSystem::new().await;
    let store = Arc::new(InMemoryEventStore::default());
    let (mock_cs, saved_with_tx) = MockCheckpointStore::new();
    let agg_id = AggregateId::new("tx-ckpt-test");

    append_tick(&store, &agg_id, 1).await;

    let notify = Arc::new(Notify::new());
    let notify_c = Arc::clone(&notify);
    let (tp, _tp_state) = MockTxProvider::new();

    // When: spawn projector with TxProvider (ExactlyOnce is forced automatically)
    let _proxy = ProjectorProps::new(
        ProjectionId::new("tx-ckpt-test-proj"),
        Arc::clone(&store) as Arc<dyn EventStore>,
        Arc::clone(&mock_cs),
        move || TxCapturingProjector {
            received_tx: Arc::new(AtomicBool::new(false)),
            notify: Arc::clone(&notify_c),
        },
    )
    .with_tx_provider(tp)
    .with_event::<TickEvent>(Arc::new(TickCodec))
    .catchup_from_aggregate(agg_id)
    .spawn(&system)
    .await;

    wait_for(&notify).await;
    // Allow the framework to complete checkpoint save after project() returns.
    tokio::task::yield_now().await;

    // Then: checkpoint was saved with Some(&mut tx)
    assert!(
        saved_with_tx.load(Ordering::SeqCst),
        "save() must be called with Some(&mut tx) when TxProvider is configured (ExactlyOnce)"
    );
}

// ---------------------------------------------------------------------------
// Test 3: TxProvider.commit() is called after a successful project()
// ---------------------------------------------------------------------------

/// After project() returns Ok, the framework must call provider.commit(tx)
/// to complete the transaction.  provider.rollback() must NOT be called.
#[tokio::test]
async fn tx_provider_commit_called_after_successful_project() {
    // Given
    let system = ProcessSystem::new().await;
    let store = Arc::new(InMemoryEventStore::default());
    let (mock_cs, _saved_flag) = MockCheckpointStore::new();
    let agg_id = AggregateId::new("tx-commit-test");

    append_tick(&store, &agg_id, 1).await;

    let notify = Arc::new(Notify::new());
    let notify_c = Arc::clone(&notify);
    let (tp, tp_state) = MockTxProvider::new();

    // When: spawn projector with TxProvider (ExactlyOnce is forced automatically)
    let _proxy = ProjectorProps::new(
        ProjectionId::new("tx-commit-test-proj"),
        Arc::clone(&store) as Arc<dyn EventStore>,
        Arc::clone(&mock_cs),
        move || TxCapturingProjector {
            received_tx: Arc::new(AtomicBool::new(false)),
            notify: Arc::clone(&notify_c),
        },
    )
    .with_tx_provider(tp)
    .with_event::<TickEvent>(Arc::new(TickCodec))
    .catchup_from_aggregate(agg_id)
    .spawn(&system)
    .await;

    wait_for(&notify).await;
    // Allow commit to complete asynchronously after project() returns.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Then: commit was called, rollback was not
    assert_eq!(
        tp_state.committed.load(Ordering::SeqCst),
        1,
        "provider.commit() must be called exactly once after a successful project()"
    );
    assert_eq!(
        tp_state.rolled_back.load(Ordering::SeqCst),
        0,
        "provider.rollback() must not be called when project() succeeds"
    );
}

// ---------------------------------------------------------------------------
// Test 4: project() failure triggers rollback, commit is not called,
//         and checkpoint is not persisted
// ---------------------------------------------------------------------------

/// When project() returns Err in ExactlyOnce + TxProvider mode, the framework
/// must call provider.rollback(tx), NOT commit.  The checkpoint store must NOT
/// be updated.
#[tokio::test]
async fn tx_provider_rollback_called_when_project_fails() {
    // Given
    let system = ProcessSystem::new().await;
    let store = Arc::new(InMemoryEventStore::default());
    let (mock_cs, saved_with_tx) = MockCheckpointStore::new();
    let agg_id = AggregateId::new("tx-rollback-test");

    append_tick(&store, &agg_id, 1).await;

    let notify = Arc::new(Notify::new());
    let notify_c = Arc::clone(&notify);
    let (tp, tp_state) = MockTxProvider::new();

    // When: spawn projector that always fails (ExactlyOnce forced by with_tx_provider)
    let _proxy = ProjectorProps::new(
        ProjectionId::new("tx-rollback-test-proj"),
        Arc::clone(&store) as Arc<dyn EventStore>,
        Arc::clone(&mock_cs),
        move || FailingProjector {
            notify: Arc::clone(&notify_c),
        },
    )
    .with_tx_provider(tp)
    .with_event::<TickEvent>(Arc::new(TickCodec))
    .catchup_from_aggregate(agg_id)
    .spawn(&system)
    .await;

    wait_for(&notify).await;
    // Allow rollback to complete asynchronously after project() returns Err.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Then: rollback was called, commit was not
    assert_eq!(
        tp_state.rolled_back.load(Ordering::SeqCst),
        1,
        "provider.rollback() must be called exactly once when project() fails"
    );
    assert_eq!(
        tp_state.committed.load(Ordering::SeqCst),
        0,
        "provider.commit() must NOT be called when project() fails"
    );

    // Then: checkpoint store was not updated
    assert!(
        !saved_with_tx.load(Ordering::SeqCst),
        "checkpoint store must not be updated when project() fails and rollback occurs"
    );
    let persisted = mock_cs
        .load(&ProjectionId::new("tx-rollback-test-proj"))
        .await
        .expect("load must succeed");
    assert_eq!(
        persisted, None,
        "checkpoint must not be persisted after a failed project + rollback"
    );
}

// ---------------------------------------------------------------------------
// Test 5: Without TxProvider, ctx.tx() returns None (baseline preserved)
// ---------------------------------------------------------------------------

/// Without a TxProvider, ctx.tx() must still return None.
/// This verifies that the default (Tx = ()) path is not affected by the T2 change.
#[tokio::test]
async fn without_tx_provider_ctx_tx_returns_none() {
    // Given: use InMemoryCheckpointStore (Tx = ()) to test the default path
    let system = ProcessSystem::new().await;
    let store = Arc::new(InMemoryEventStore::default());
    let agg_id = AggregateId::new("tx-none-test");
    use nitinol_persistence::store::InMemoryCheckpointStore;
    let cs = Arc::new(InMemoryCheckpointStore::default());

    append_tick(&store, &agg_id, 1).await;

    let tx_was_some = Arc::new(AtomicBool::new(false));
    let notify = Arc::new(Notify::new());
    let tx_was_some_c = Arc::clone(&tx_was_some);
    let notify_c = Arc::clone(&notify);

    // A projector that works with Tx = ()
    struct NullTxProjector {
        tx_was_some: Arc<AtomicBool>,
        notify: Arc<Notify>,
    }

    #[async_trait]
    impl Projector<TickEvent> for NullTxProjector {
        type Error = std::convert::Infallible;

        async fn project(
            &mut self,
            _event: TickEvent,
            ctx: &mut ProjectionContext<'_, ()>,
        ) -> Result<(), Self::Error> {
            if ctx.tx().is_some() {
                self.tx_was_some.store(true, Ordering::SeqCst);
            }
            self.notify.notify_one();
            Ok(())
        }
    }

    // When: spawn without TxProvider (default Tx = ())
    let _proxy = ProjectorProps::new(
        ProjectionId::new("tx-none-test-proj"),
        Arc::clone(&store) as Arc<dyn EventStore>,
        cs,
        move || NullTxProjector {
            tx_was_some: Arc::clone(&tx_was_some_c),
            notify: Arc::clone(&notify_c),
        },
    )
    .with_event::<TickEvent>(Arc::new(TickCodec))
    // No with_tx_provider() call — default Tx = ()
    .catchup_from_aggregate(agg_id)
    .spawn(&system)
    .await;

    wait_for(&notify).await;

    // Then: ctx.tx() was None (no TxProvider configured)
    assert!(
        !tx_was_some.load(Ordering::SeqCst),
        "ctx.tx() must return None when no TxProvider is configured"
    );
}
