// Integration tests for the subscription cursor: it must not move past an
// event whose application did not succeed.
//
// The invariant under test is owned jointly by `projection/delivery.rs` (which
// classifies the outcome of one application attempt) and
// `projection/process.rs` (which projects that outcome into the reply the
// `DirectPoller` observes).  `durable_stream/poller.rs` already keeps the
// cursor in place when `ask` returns `Err`, so every assertion here is written
// against the only externally observable consequences: which sequences reach
// `project`, in what order, and what the offset store holds.
//
// Contract coverage (IDs from the plan report):
//
// | Test                                                        | Contract          |
// |-------------------------------------------------------------|-------------------|
// | at_least_once_failing_event_blocks_the_next_event            | C-ALO-HOLD        |
// | at_least_once_next_event_waits_for_the_retry_to_succeed      | C-ALO-HOLD        |
// | at_least_once_failed_event_survives_a_restart                | C-ALO-RESTART     |
// | exactly_once_tx_project_failure_blocks_the_next_event        | C-EO-TX-PROJECT   |
// | exactly_once_tx_begin_failure_blocks_the_next_event          | C-EO-TX-BEGIN     |
// | exactly_once_tx_offset_save_failure_blocks_the_next_event| C-EO-TX-SAVE      |
// | exactly_once_tx_commit_failure_blocks_the_next_event         | C-EO-TX-COMMIT    |
// | at_most_once_failing_event_does_not_block_the_next_event     | C-AMO-KEEP        |
// | exactly_once_without_tx_project_failure_blocks_next_event    | C-EO-NOTX-DEF     |
// | projector_keeps_retrying_a_permanently_failing_event         | C-SUPERVISION     |
// | unregistered_event_type_does_not_block_the_next_event        | C-SKIP-KEEP       |
// | already_processed_event_is_accepted_without_projecting        | C-SKIP-KEEP       |

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;

use nitinol_eventsource::{
    codec::Codec, Event, ProjectionContext, ProjectionId, Projector, ProjectorProps, TxProvider,
};
use nitinol_persistence::error::OffsetError;
use nitinol_persistence::provider::{CustomStore, PersistenceProvider};
use nitinol_persistence::store::{
    DeliveryMode, EventStore, InMemoryEventStore, InMemoryOffsetStore, OffsetScope, OffsetStore,
};
use nitinol_persistence::{AggregateId, AppendingEvent, EventType, Family, LoadedEvent, TypeName};
use nitinol_runtime::ProcessSystem;

// Timing
//
// A projector polls its event store every `DEFAULT_POLL_INTERVAL` (250 ms,
// `durable_stream.rs:19`) and that interval cannot be overridden through
// `ProjectorProps`.  The longest chain asserted here needs four ticks, so the
// bound below leaves room for a loaded machine without ever being the thing a
// passing test waits on: every wait returns as soon as its condition holds.

const WAIT_TIMEOUT: Duration = Duration::from_secs(5);
const POLL_STEP: Duration = Duration::from_millis(5);

/// Wait until `condition` holds. Returns `false` if [`WAIT_TIMEOUT`] elapses
/// first, so the caller can assert on the observed state instead of dying with
/// a bare timeout panic.
async fn wait_for(mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + WAIT_TIMEOUT;
    loop {
        if condition() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(POLL_STEP).await;
    }
}

async fn offset_of<Cs: OffsetStore>(store: &Arc<Cs>, id: &ProjectionId) -> Option<u64> {
    store
        .load(&OffsetScope::from(id))
        .await
        .expect("offset load must succeed")
}

/// Wait until the persisted offset equals `expected`.
async fn wait_for_offset<Cs: OffsetStore>(
    store: &Arc<Cs>,
    id: &ProjectionId,
    expected: Option<u64>,
) -> bool {
    let deadline = Instant::now() + WAIT_TIMEOUT;
    loop {
        if offset_of(store, id).await == expected {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(POLL_STEP).await;
    }
}

/// Wait until a stopped process has left the registry, so a second projector
/// over the same stores cannot race the first one.
async fn wait_for_deregister(system: &ProcessSystem, pid: nitinol_runtime::ident::Pid) -> bool {
    let deadline = Instant::now() + WAIT_TIMEOUT;
    loop {
        if system.lookup(pid).await.is_none() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(POLL_STEP).await;
    }
}

// Fixtures: event type + codec

#[derive(Clone)]
struct Evt;

impl Event for Evt {
    const EVENT_TYPE: EventType = EventType::new(Family::new("issue109"), TypeName::new("Evt"));

    type Codec = UnitCodec;
}

/// An event type that is never registered via `with_event`, used to pin down
/// that "not applied" and "must be retried" are different classifications.
const UNREGISTERED_EVENT_TYPE: EventType =
    EventType::new(Family::new("issue109"), TypeName::new("Unregistered"));

struct UnitCodec;

impl Codec<Evt> for UnitCodec {
    type Error = std::convert::Infallible;

    fn encode(_event: &Evt) -> Result<Bytes, Self::Error> {
        Ok(Bytes::new())
    }

    fn decode(_payload: &[u8]) -> Result<Evt, Self::Error> {
        Ok(Evt)
    }
}

async fn append(
    store: &InMemoryEventStore,
    agg_id: &AggregateId,
    sequence: u64,
    event_type: EventType,
) {
    store
        .append(
            agg_id.as_str(),
            vec![AppendingEvent {
                sequence,
                event_type,
                payload: Bytes::new(),
                occurred_at: jiff::Timestamp::now(),
            }],
        )
        .await
        .expect("append must succeed");
}

async fn append_evt(store: &InMemoryEventStore, agg_id: &AggregateId, sequence: u64) {
    append(store, agg_id, sequence, Evt::EVENT_TYPE).await;
}

// Fixtures: failure budgets
//
// Every fixture failure is drawn from a shared budget rather than from a
// per-attempt flag, so a test states "fail the first N attempts" once and the
// resulting call sequence is independent of how the poll ticks land.

/// A budget that is never exhausted.
const PERMANENT: usize = usize::MAX;

fn budget(failures: usize) -> Arc<AtomicUsize> {
    Arc::new(AtomicUsize::new(failures))
}

/// Consume one failure from `remaining`; `false` once the budget is spent.
fn take_failure(remaining: &AtomicUsize) -> bool {
    remaining
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| match n {
            0 => None,
            PERMANENT => Some(PERMANENT),
            n => Some(n - 1),
        })
        .is_ok()
}

// Fixtures: attempt recorder

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Attempt {
    sequence: u64,
    succeeded: bool,
}

/// Records every `project` call in order. The order — not just the count — is
/// what distinguishes "the cursor waited for seq=1" from "the cursor moved on
/// and seq=1 was lost".
#[derive(Default)]
struct Recorder {
    attempts: Mutex<Vec<Attempt>>,
}

impl Recorder {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn record(&self, sequence: u64, succeeded: bool) {
        self.lock().push(Attempt {
            sequence,
            succeeded,
        });
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Attempt>> {
        self.attempts
            .lock()
            .expect("recorder mutex is never poisoned: recording cannot panic")
    }

    fn attempts(&self) -> Vec<Attempt> {
        self.lock().clone()
    }

    fn sequences(&self) -> Vec<u64> {
        self.lock().iter().map(|a| a.sequence).collect()
    }

    fn len(&self) -> usize {
        self.lock().len()
    }

    fn clear(&self) {
        self.lock().clear();
    }
}

// Fixtures: projectors

#[derive(Debug, thiserror::Error)]
#[error("intentional project failure")]
struct ProjectFailed;

/// Records each attempt and fails `fail_sequence` until its budget runs out.
struct RecordingProjector {
    recorder: Arc<Recorder>,
    fail_sequence: Option<u64>,
    remaining_failures: Arc<AtomicUsize>,
}

#[async_trait]
impl Projector<Evt> for RecordingProjector {
    type Error = ProjectFailed;

    async fn project(
        &mut self,
        _event: Evt,
        ctx: &mut ProjectionContext<'_, ()>,
    ) -> Result<(), Self::Error> {
        let sequence = ctx.current_sequence();
        let failing =
            self.fail_sequence == Some(sequence) && take_failure(&self.remaining_failures);
        self.recorder.record(sequence, !failing);
        if failing {
            Err(ProjectFailed)
        } else {
            Ok(())
        }
    }
}

/// The `Tx = MockTx` counterpart of [`RecordingProjector`].
struct RecordingTxProjector {
    recorder: Arc<Recorder>,
    fail_sequence: Option<u64>,
    remaining_failures: Arc<AtomicUsize>,
}

#[async_trait]
impl Projector<Evt, MockTx> for RecordingTxProjector {
    type Error = ProjectFailed;

    async fn project(
        &mut self,
        _event: Evt,
        ctx: &mut ProjectionContext<'_, MockTx>,
    ) -> Result<(), Self::Error> {
        let sequence = ctx.current_sequence();
        let failing =
            self.fail_sequence == Some(sequence) && take_failure(&self.remaining_failures);
        self.recorder.record(sequence, !failing);
        if failing {
            Err(ProjectFailed)
        } else {
            Ok(())
        }
    }
}

// Fixtures: transaction provider

struct MockTx;

#[derive(Debug, thiserror::Error)]
#[error("intentional transaction failure")]
struct TxFailed;

/// Counts every `begin` / `commit` / `rollback`, and fails `begin` or `commit`
/// while the corresponding budget lasts.
struct CountingTxProvider {
    begun: Arc<AtomicU64>,
    committed: Arc<AtomicU64>,
    rolled_back: Arc<AtomicU64>,
    begin_failures: Arc<AtomicUsize>,
    commit_failures: Arc<AtomicUsize>,
}

/// The observable half of [`CountingTxProvider`], kept by the test after the
/// provider itself has been moved into the projector.
struct TxCounters {
    begun: Arc<AtomicU64>,
    committed: Arc<AtomicU64>,
    rolled_back: Arc<AtomicU64>,
}

impl TxCounters {
    fn begun(&self) -> u64 {
        self.begun.load(Ordering::SeqCst)
    }

    fn committed(&self) -> u64 {
        self.committed.load(Ordering::SeqCst)
    }

    fn rolled_back(&self) -> u64 {
        self.rolled_back.load(Ordering::SeqCst)
    }
}

impl CountingTxProvider {
    fn new(begin_failures: usize, commit_failures: usize) -> (Self, TxCounters) {
        let begun = Arc::new(AtomicU64::new(0));
        let committed = Arc::new(AtomicU64::new(0));
        let rolled_back = Arc::new(AtomicU64::new(0));
        let provider = CountingTxProvider {
            begun: Arc::clone(&begun),
            committed: Arc::clone(&committed),
            rolled_back: Arc::clone(&rolled_back),
            begin_failures: budget(begin_failures),
            commit_failures: budget(commit_failures),
        };
        let counters = TxCounters {
            begun,
            committed,
            rolled_back,
        };
        (provider, counters)
    }
}

#[async_trait]
impl TxProvider for CountingTxProvider {
    type Tx = MockTx;
    type Error = TxFailed;

    async fn begin(&self) -> Result<MockTx, Self::Error> {
        self.begun.fetch_add(1, Ordering::SeqCst);
        if take_failure(&self.begin_failures) {
            return Err(TxFailed);
        }
        Ok(MockTx)
    }

    async fn commit(&self, _tx: MockTx) -> Result<(), Self::Error> {
        self.committed.fetch_add(1, Ordering::SeqCst);
        if take_failure(&self.commit_failures) {
            return Err(TxFailed);
        }
        Ok(())
    }

    async fn rollback(&self, _tx: MockTx) {
        self.rolled_back.fetch_add(1, Ordering::SeqCst);
    }
}

// Fixtures: offset store with Tx = MockTx

/// A `OffsetStore` whose `Tx` matches [`CountingTxProvider`] and whose
/// `save` fails while its budget lasts.
struct MockOffsetStore {
    state: Mutex<HashMap<String, u64>>,
    saves: Arc<AtomicU64>,
    save_failures: Arc<AtomicUsize>,
}

impl MockOffsetStore {
    fn new(save_failures: usize) -> (Arc<Self>, Arc<AtomicU64>) {
        let saves = Arc::new(AtomicU64::new(0));
        let store = Arc::new(Self {
            state: Mutex::new(HashMap::new()),
            saves: Arc::clone(&saves),
            save_failures: budget(save_failures),
        });
        (store, saves)
    }
}

#[async_trait]
impl OffsetStore for MockOffsetStore {
    type Tx = MockTx;

    async fn load(&self, scope: &OffsetScope) -> Result<Option<u64>, OffsetError> {
        Ok(self
            .state
            .lock()
            .expect("offset mutex is never poisoned: no store operation panics while held")
            .get(scope.as_str())
            .copied())
    }

    async fn save(
        &self,
        scope: &OffsetScope,
        sequence: u64,
        _tx: Option<&mut Self::Tx>,
    ) -> Result<(), OffsetError> {
        self.saves.fetch_add(1, Ordering::SeqCst);
        if take_failure(&self.save_failures) {
            return Err(OffsetError::Backend(Box::new(std::io::Error::other(
                "intentional offset save failure",
            ))));
        }
        self.state
            .lock()
            .expect("offset mutex is never poisoned: no store operation panics while held")
            .insert(scope.as_str().to_owned(), sequence);
        Ok(())
    }
}

// C-ALO-HOLD: a permanently failing event blocks everything behind it.

/// seq=1 never succeeds, so seq=2 must never reach `project` and the
/// offset must never be persisted.
///
/// A single-event test cannot see this: it only shows that the offset does
/// not advance *at that moment*.  The defect in #109 is that the cursor moves
/// on regardless, so the failing event is overtaken by the next one and never
/// redelivered.
#[tokio::test(flavor = "multi_thread")]
async fn at_least_once_failing_event_blocks_the_next_event() {
    let system = ProcessSystem::new().await;
    let event_store = Arc::new(InMemoryEventStore::default());
    let offset_store = Arc::new(InMemoryOffsetStore::default());
    let agg_id = AggregateId::new("alo-hold-agg");
    let projection_id = ProjectionId::new("alo-hold");

    append_evt(&event_store, &agg_id, 1).await;
    append_evt(&event_store, &agg_id, 2).await;

    let recorder = Recorder::new();
    let failures = budget(PERMANENT);

    let recorder_c = Arc::clone(&recorder);
    let failures_c = Arc::clone(&failures);
    let persistence = PersistenceProvider::new(
        CustomStore::builder()
            .eventstore(Arc::clone(&event_store) as Arc<dyn EventStore>)
            .projection_offsets(Arc::clone(&offset_store))
            .build(),
    );
    let _proxy = ProjectorProps::new(projection_id.clone(), &persistence, move || {
        RecordingProjector {
            recorder: Arc::clone(&recorder_c),
            fail_sequence: Some(1),
            remaining_failures: Arc::clone(&failures_c),
        }
    })
    .with_event::<Evt>()
    .delivery_mode(DeliveryMode::AtLeastOnce)
    .catchup_from_aggregate(agg_id)
    .spawn(&system)
    .await;

    let retried = wait_for(|| recorder.len() >= 3).await;
    let observed = recorder.sequences();
    let offset = offset_of(&offset_store, &projection_id).await;

    assert!(
        !observed.contains(&2),
        "seq=2 must not be projected while seq=1 keeps failing; observed {observed:?}"
    );
    assert_eq!(
        offset, None,
        "the offset must not advance past a failing event; observed {observed:?}"
    );
    assert!(
        retried,
        "seq=1 must keep being redelivered while it fails; observed {observed:?}"
    );
}

// C-ALO-HOLD: the acceptance scenario, as a two-event sequence.

/// seq=1 fails once and succeeds on redelivery. seq=2 must not be applied
/// before that success, and the offset must end at 2.
#[tokio::test(flavor = "multi_thread")]
async fn at_least_once_next_event_waits_for_the_retry_to_succeed() {
    let system = ProcessSystem::new().await;
    let event_store = Arc::new(InMemoryEventStore::default());
    let offset_store = Arc::new(InMemoryOffsetStore::default());
    let agg_id = AggregateId::new("alo-retry-agg");
    let projection_id = ProjectionId::new("alo-retry");

    append_evt(&event_store, &agg_id, 1).await;
    append_evt(&event_store, &agg_id, 2).await;

    let recorder = Recorder::new();
    let failures = budget(1);

    let recorder_c = Arc::clone(&recorder);
    let failures_c = Arc::clone(&failures);
    let persistence = PersistenceProvider::new(
        CustomStore::builder()
            .eventstore(Arc::clone(&event_store) as Arc<dyn EventStore>)
            .projection_offsets(Arc::clone(&offset_store))
            .build(),
    );
    let _proxy = ProjectorProps::new(projection_id.clone(), &persistence, move || {
        RecordingProjector {
            recorder: Arc::clone(&recorder_c),
            fail_sequence: Some(1),
            remaining_failures: Arc::clone(&failures_c),
        }
    })
    .with_event::<Evt>()
    .delivery_mode(DeliveryMode::AtLeastOnce)
    .catchup_from_aggregate(agg_id)
    .spawn(&system)
    .await;

    let reached = wait_for_offset(&offset_store, &projection_id, Some(2)).await;
    let observed = recorder.attempts();

    assert_eq!(
        observed,
        vec![
            Attempt {
                sequence: 1,
                succeeded: false
            },
            Attempt {
                sequence: 1,
                succeeded: true
            },
            Attempt {
                sequence: 2,
                succeeded: true
            },
        ],
        "seq=2 must only be applied after seq=1 has succeeded"
    );
    assert!(
        reached,
        "the offset must reach 2 once both events have been applied; observed {observed:?}"
    );
}

// C-ALO-RESTART: a failing event survives a projector restart.

/// While seq=1 keeps failing the offset stays unset, so a projector
/// restarted over the same stores redelivers seq=1 rather than skipping it.
#[tokio::test(flavor = "multi_thread")]
async fn at_least_once_failed_event_survives_a_restart() {
    let system = ProcessSystem::new().await;
    let event_store = Arc::new(InMemoryEventStore::default());
    let offset_store = Arc::new(InMemoryOffsetStore::default());
    let agg_id = AggregateId::new("alo-restart-agg");
    let projection_id = ProjectionId::new("alo-restart");

    append_evt(&event_store, &agg_id, 1).await;
    append_evt(&event_store, &agg_id, 2).await;

    let recorder = Recorder::new();
    let failures = budget(PERMANENT);

    let first_run = {
        let recorder_c = Arc::clone(&recorder);
        let failures_c = Arc::clone(&failures);
        let persistence = PersistenceProvider::new(
            CustomStore::builder()
                .eventstore(Arc::clone(&event_store) as Arc<dyn EventStore>)
                .projection_offsets(Arc::clone(&offset_store))
                .build(),
        );
        ProjectorProps::new(projection_id.clone(), &persistence, move || {
            RecordingProjector {
                recorder: Arc::clone(&recorder_c),
                fail_sequence: Some(1),
                remaining_failures: Arc::clone(&failures_c),
            }
        })
        .with_event::<Evt>()
        .delivery_mode(DeliveryMode::AtLeastOnce)
        .catchup_from_aggregate(agg_id.clone())
        .spawn(&system)
        .await
    };

    let retried = wait_for(|| recorder.len() >= 2).await;
    let before_restart = recorder.sequences();
    let offset_before_restart = offset_of(&offset_store, &projection_id).await;

    assert!(
        !before_restart.contains(&2),
        "seq=2 must not be projected while seq=1 keeps failing; observed {before_restart:?}"
    );
    assert_eq!(
        offset_before_restart, None,
        "the offset must not advance past a failing event; observed {before_restart:?}"
    );
    assert!(
        retried,
        "seq=1 must keep being redelivered while it fails; observed {before_restart:?}"
    );

    let first_pid = first_run.pid();
    let _ = first_run.stop().await;
    assert!(
        wait_for_deregister(&system, first_pid).await,
        "the first projector must deregister before the second one starts"
    );

    // Second run: the same stores, with the failure budget spent.
    failures.store(0, Ordering::SeqCst);
    recorder.clear();

    let recorder_c = Arc::clone(&recorder);
    let failures_c = Arc::clone(&failures);
    let persistence = PersistenceProvider::new(
        CustomStore::builder()
            .eventstore(Arc::clone(&event_store) as Arc<dyn EventStore>)
            .projection_offsets(Arc::clone(&offset_store))
            .build(),
    );
    let _second_run = ProjectorProps::new(projection_id.clone(), &persistence, move || {
        RecordingProjector {
            recorder: Arc::clone(&recorder_c),
            fail_sequence: Some(1),
            remaining_failures: Arc::clone(&failures_c),
        }
    })
    .with_event::<Evt>()
    .delivery_mode(DeliveryMode::AtLeastOnce)
    .catchup_from_aggregate(agg_id)
    .spawn(&system)
    .await;

    let reached = wait_for_offset(&offset_store, &projection_id, Some(2)).await;
    let after_restart = recorder.sequences();

    assert_eq!(
        after_restart,
        vec![1, 2],
        "the restarted projector must redeliver seq=1 before seq=2"
    );
    assert!(
        reached,
        "the offset must reach 2 after the restart; observed {after_restart:?}"
    );
}

// C-EO-TX-PROJECT: rollback alone does not authorise the cursor to move.

#[tokio::test(flavor = "multi_thread")]
async fn exactly_once_tx_project_failure_blocks_the_next_event() {
    let system = ProcessSystem::new().await;
    let event_store = Arc::new(InMemoryEventStore::default());
    let (offset_store, saves) = MockOffsetStore::new(0);
    let agg_id = AggregateId::new("eo-tx-project-agg");
    let projection_id = ProjectionId::new("eo-tx-project");

    append_evt(&event_store, &agg_id, 1).await;
    append_evt(&event_store, &agg_id, 2).await;

    let recorder = Recorder::new();
    let failures = budget(PERMANENT);
    let (provider, counters) = CountingTxProvider::new(0, 0);

    let recorder_c = Arc::clone(&recorder);
    let failures_c = Arc::clone(&failures);
    let persistence = PersistenceProvider::new(
        CustomStore::builder()
            .eventstore(Arc::clone(&event_store) as Arc<dyn EventStore>)
            .projection_offsets(Arc::clone(&offset_store))
            .build(),
    );
    let _proxy = ProjectorProps::new(projection_id.clone(), &persistence, move || {
        RecordingTxProjector {
            recorder: Arc::clone(&recorder_c),
            fail_sequence: Some(1),
            remaining_failures: Arc::clone(&failures_c),
        }
    })
    .with_tx_provider(provider)
    .with_event::<Evt>()
    .catchup_from_aggregate(agg_id)
    .spawn(&system)
    .await;

    let retried = wait_for(|| recorder.len() >= 3).await;
    let observed = recorder.sequences();
    let offset = offset_of(&offset_store, &projection_id).await;

    assert!(
        !observed.contains(&2),
        "seq=2 must not be projected while seq=1 keeps failing; observed {observed:?}"
    );
    assert_eq!(
        offset, None,
        "a rolled-back event must leave no offset; observed {observed:?}"
    );
    assert_eq!(
        saves.load(Ordering::SeqCst),
        0,
        "the offset save must not be attempted when project() fails"
    );
    assert_eq!(
        counters.committed(),
        0,
        "commit must not be called when project() fails"
    );
    assert!(
        counters.rolled_back() >= 1,
        "each failed project() must roll its transaction back"
    );
    assert!(
        retried,
        "seq=1 must keep being redelivered while it fails; observed {observed:?}"
    );
}

// C-EO-TX-BEGIN: a transaction that never started leaves the event unapplied.

/// `begin` fails twice and then succeeds. seq=1 must still be the first event
/// ever handed to `project` — a failed `begin` is not "nothing to do for this
/// event".
#[tokio::test(flavor = "multi_thread")]
async fn exactly_once_tx_begin_failure_blocks_the_next_event() {
    let system = ProcessSystem::new().await;
    let event_store = Arc::new(InMemoryEventStore::default());
    let (offset_store, _saves) = MockOffsetStore::new(0);
    let agg_id = AggregateId::new("eo-tx-begin-agg");
    let projection_id = ProjectionId::new("eo-tx-begin");

    append_evt(&event_store, &agg_id, 1).await;
    append_evt(&event_store, &agg_id, 2).await;

    let recorder = Recorder::new();
    let (provider, counters) = CountingTxProvider::new(2, 0);

    let recorder_c = Arc::clone(&recorder);
    let persistence = PersistenceProvider::new(
        CustomStore::builder()
            .eventstore(Arc::clone(&event_store) as Arc<dyn EventStore>)
            .projection_offsets(Arc::clone(&offset_store))
            .build(),
    );
    let _proxy = ProjectorProps::new(projection_id.clone(), &persistence, move || {
        RecordingTxProjector {
            recorder: Arc::clone(&recorder_c),
            fail_sequence: None,
            remaining_failures: budget(0),
        }
    })
    .with_tx_provider(provider)
    .with_event::<Evt>()
    .catchup_from_aggregate(agg_id)
    .spawn(&system)
    .await;

    let reached = wait_for_offset(&offset_store, &projection_id, Some(2)).await;
    let observed = recorder.sequences();

    assert_eq!(
        observed,
        vec![1, 2],
        "a failed begin() must leave seq=1 pending, not consumed (begin calls: {})",
        counters.begun()
    );
    assert!(
        counters.begun() >= 3,
        "begin() must be retried for seq=1 after each failure; observed {} calls",
        counters.begun()
    );
    assert!(
        reached,
        "the offset must reach 2 once begin() recovers; observed {observed:?}"
    );
}

// C-EO-TX-SAVE: an offset save that failed does not make the event processed.

/// The offset save fails twice and then succeeds. A successful `project`
/// is not enough to let the cursor move when the save inside the same
/// transaction failed and the transaction was rolled back.
#[tokio::test(flavor = "multi_thread")]
async fn exactly_once_tx_offset_save_failure_blocks_the_next_event() {
    let system = ProcessSystem::new().await;
    let event_store = Arc::new(InMemoryEventStore::default());
    let (offset_store, _saves) = MockOffsetStore::new(2);
    let agg_id = AggregateId::new("eo-tx-save-agg");
    let projection_id = ProjectionId::new("eo-tx-save");

    append_evt(&event_store, &agg_id, 1).await;
    append_evt(&event_store, &agg_id, 2).await;

    let recorder = Recorder::new();
    let (provider, counters) = CountingTxProvider::new(0, 0);

    let recorder_c = Arc::clone(&recorder);
    let persistence = PersistenceProvider::new(
        CustomStore::builder()
            .eventstore(Arc::clone(&event_store) as Arc<dyn EventStore>)
            .projection_offsets(Arc::clone(&offset_store))
            .build(),
    );
    let _proxy = ProjectorProps::new(projection_id.clone(), &persistence, move || {
        RecordingTxProjector {
            recorder: Arc::clone(&recorder_c),
            fail_sequence: None,
            remaining_failures: budget(0),
        }
    })
    .with_tx_provider(provider)
    .with_event::<Evt>()
    .catchup_from_aggregate(agg_id)
    .spawn(&system)
    .await;

    let reached = wait_for_offset(&offset_store, &projection_id, Some(2)).await;
    let observed = recorder.sequences();

    assert_eq!(
        observed,
        vec![1, 1, 1, 2],
        "seq=1 must be re-applied until its offset save succeeds, before seq=2"
    );
    assert_eq!(
        counters.rolled_back(),
        2,
        "each failed offset save must roll its transaction back"
    );
    assert!(
        reached,
        "the offset must reach 2 once the save recovers; observed {observed:?}"
    );
}

// C-EO-TX-COMMIT: an unknown commit outcome is treated as uncommitted.

/// `commit` fails on every attempt. Its outcome is indistinguishable from the
/// framework's side — the transaction was already consumed by `commit`, so
/// there is nothing left to roll back — and the safe reading is "not
/// committed": the cursor holds and seq=1 is redelivered.
#[tokio::test(flavor = "multi_thread")]
async fn exactly_once_tx_commit_failure_blocks_the_next_event() {
    let system = ProcessSystem::new().await;
    let event_store = Arc::new(InMemoryEventStore::default());
    let (offset_store, _saves) = MockOffsetStore::new(0);
    let agg_id = AggregateId::new("eo-tx-commit-agg");
    let projection_id = ProjectionId::new("eo-tx-commit");

    append_evt(&event_store, &agg_id, 1).await;
    append_evt(&event_store, &agg_id, 2).await;

    let recorder = Recorder::new();
    let (provider, counters) = CountingTxProvider::new(0, PERMANENT);

    let recorder_c = Arc::clone(&recorder);
    let persistence = PersistenceProvider::new(
        CustomStore::builder()
            .eventstore(Arc::clone(&event_store) as Arc<dyn EventStore>)
            .projection_offsets(Arc::clone(&offset_store))
            .build(),
    );
    let _proxy = ProjectorProps::new(projection_id.clone(), &persistence, move || {
        RecordingTxProjector {
            recorder: Arc::clone(&recorder_c),
            fail_sequence: None,
            remaining_failures: budget(0),
        }
    })
    .with_tx_provider(provider)
    .with_event::<Evt>()
    .catchup_from_aggregate(agg_id)
    .spawn(&system)
    .await;

    let retried = wait_for(|| recorder.len() >= 3).await;
    let observed = recorder.sequences();

    assert!(
        !observed.contains(&2),
        "seq=2 must not be projected while seq=1's commit keeps failing; observed {observed:?}"
    );
    assert_eq!(
        counters.rolled_back(),
        0,
        "rollback must not be attempted after commit consumed the transaction"
    );
    assert!(
        counters.begun() >= 3,
        "seq=1 must be redelivered with a fresh transaction after each failed commit; \
         observed {} begin calls",
        counters.begun()
    );
    assert!(
        retried,
        "seq=1 must keep being redelivered while its commit fails; observed {observed:?}"
    );
}

// C-AMO-KEEP: AtMostOnce keeps its existing semantics.

/// AtMostOnce saves the offset before `project` and treats a failed
/// handler as delivered. The fix for #109 must not be applied uniformly to
/// every delivery mode: seq=2 must still be applied after seq=1 fails.
#[tokio::test(flavor = "multi_thread")]
async fn at_most_once_failing_event_does_not_block_the_next_event() {
    let system = ProcessSystem::new().await;
    let event_store = Arc::new(InMemoryEventStore::default());
    let offset_store = Arc::new(InMemoryOffsetStore::default());
    let agg_id = AggregateId::new("amo-keep-agg");
    let projection_id = ProjectionId::new("amo-keep");

    append_evt(&event_store, &agg_id, 1).await;
    append_evt(&event_store, &agg_id, 2).await;

    let recorder = Recorder::new();
    let failures = budget(PERMANENT);

    let recorder_c = Arc::clone(&recorder);
    let failures_c = Arc::clone(&failures);
    let persistence = PersistenceProvider::new(
        CustomStore::builder()
            .eventstore(Arc::clone(&event_store) as Arc<dyn EventStore>)
            .projection_offsets(Arc::clone(&offset_store))
            .build(),
    );
    let _proxy = ProjectorProps::new(projection_id.clone(), &persistence, move || {
        RecordingProjector {
            recorder: Arc::clone(&recorder_c),
            fail_sequence: Some(1),
            remaining_failures: Arc::clone(&failures_c),
        }
    })
    .with_event::<Evt>()
    .delivery_mode(DeliveryMode::AtMostOnce)
    .catchup_from_aggregate(agg_id)
    .spawn(&system)
    .await;

    let reached = wait_for_offset(&offset_store, &projection_id, Some(2)).await;
    let observed = recorder.sequences();

    assert_eq!(
        observed,
        vec![1, 2],
        "AtMostOnce must move past a failed event instead of redelivering it"
    );
    assert!(
        reached,
        "AtMostOnce must advance the offset to 2; observed {observed:?}"
    );
}

// C-EO-NOTX-DEF: ExactlyOnce without a TxProvider.

/// Without a TxProvider the only failure signal the framework has is the
/// `Result` of `project`. An `Err` is a failure, so the cursor holds — even
/// though the framework still saves no offset in this mode.
#[tokio::test(flavor = "multi_thread")]
async fn exactly_once_without_tx_project_failure_blocks_the_next_event() {
    let system = ProcessSystem::new().await;
    let event_store = Arc::new(InMemoryEventStore::default());
    let offset_store = Arc::new(InMemoryOffsetStore::default());
    let agg_id = AggregateId::new("eo-notx-agg");
    let projection_id = ProjectionId::new("eo-notx");

    append_evt(&event_store, &agg_id, 1).await;
    append_evt(&event_store, &agg_id, 2).await;

    let recorder = Recorder::new();
    let failures = budget(PERMANENT);

    let recorder_c = Arc::clone(&recorder);
    let failures_c = Arc::clone(&failures);
    let persistence = PersistenceProvider::new(
        CustomStore::builder()
            .eventstore(Arc::clone(&event_store) as Arc<dyn EventStore>)
            .projection_offsets(Arc::clone(&offset_store))
            .build(),
    );
    let _proxy = ProjectorProps::new(projection_id.clone(), &persistence, move || {
        RecordingProjector {
            recorder: Arc::clone(&recorder_c),
            fail_sequence: Some(1),
            remaining_failures: Arc::clone(&failures_c),
        }
    })
    .with_event::<Evt>()
    .delivery_mode(DeliveryMode::ExactlyOnce)
    .catchup_from_aggregate(agg_id)
    .spawn(&system)
    .await;

    let retried = wait_for(|| recorder.len() >= 3).await;
    let observed = recorder.sequences();
    let offset = offset_of(&offset_store, &projection_id).await;

    assert!(
        !observed.contains(&2),
        "seq=2 must not be projected while seq=1 keeps failing; observed {observed:?}"
    );
    assert_eq!(
        offset, None,
        "ExactlyOnce leaves the offset to the user; the framework must not save it"
    );
    assert!(
        retried,
        "seq=1 must keep being redelivered while it fails; observed {observed:?}"
    );
}

// C-SUPERVISION: holding the cursor must not cost the projector its life.

/// A failure that is reported upstream also reaches supervision. If that stops
/// the projector, the held cursor never produces a retry and the projection is
/// dead rather than paused.
#[tokio::test(flavor = "multi_thread")]
async fn projector_keeps_retrying_a_permanently_failing_event() {
    let system = ProcessSystem::new().await;
    let event_store = Arc::new(InMemoryEventStore::default());
    let offset_store = Arc::new(InMemoryOffsetStore::default());
    let agg_id = AggregateId::new("supervision-agg");
    let projection_id = ProjectionId::new("supervision");

    append_evt(&event_store, &agg_id, 1).await;

    let recorder = Recorder::new();
    let failures = budget(PERMANENT);

    let recorder_c = Arc::clone(&recorder);
    let failures_c = Arc::clone(&failures);
    let persistence = PersistenceProvider::new(
        CustomStore::builder()
            .eventstore(Arc::clone(&event_store) as Arc<dyn EventStore>)
            .projection_offsets(Arc::clone(&offset_store))
            .build(),
    );
    let proxy = ProjectorProps::new(projection_id.clone(), &persistence, move || {
        RecordingProjector {
            recorder: Arc::clone(&recorder_c),
            fail_sequence: Some(1),
            remaining_failures: Arc::clone(&failures_c),
        }
    })
    .with_event::<Evt>()
    .delivery_mode(DeliveryMode::AtLeastOnce)
    .catchup_from_aggregate(agg_id)
    .spawn(&system)
    .await;

    let retried = wait_for(|| recorder.len() >= 3).await;
    let attempts = recorder.len();
    let alive = system.lookup(proxy.pid()).await.is_some();

    assert!(
        alive,
        "the projector must stay alive while an event keeps failing; {attempts} attempts observed"
    );
    assert!(
        retried,
        "a held event must be retried on every poll; {attempts} attempts observed"
    );
}

// C-SKIP-KEEP: "not applied" is not the same as "must be retried".

/// An event of an unregistered type has no handler to fail. It is skipped, and
/// the events behind it must still be delivered.
#[tokio::test(flavor = "multi_thread")]
async fn unregistered_event_type_does_not_block_the_next_event() {
    let system = ProcessSystem::new().await;
    let event_store = Arc::new(InMemoryEventStore::default());
    let offset_store = Arc::new(InMemoryOffsetStore::default());
    let agg_id = AggregateId::new("skip-agg");
    let projection_id = ProjectionId::new("skip");

    append(&event_store, &agg_id, 1, UNREGISTERED_EVENT_TYPE).await;
    append_evt(&event_store, &agg_id, 2).await;

    let recorder = Recorder::new();

    let recorder_c = Arc::clone(&recorder);
    let persistence = PersistenceProvider::new(
        CustomStore::builder()
            .eventstore(Arc::clone(&event_store) as Arc<dyn EventStore>)
            .projection_offsets(Arc::clone(&offset_store))
            .build(),
    );
    let _proxy = ProjectorProps::new(projection_id.clone(), &persistence, move || {
        RecordingProjector {
            recorder: Arc::clone(&recorder_c),
            fail_sequence: None,
            remaining_failures: budget(0),
        }
    })
    .with_event::<Evt>()
    .delivery_mode(DeliveryMode::AtLeastOnce)
    .catchup_from_aggregate(agg_id)
    .spawn(&system)
    .await;

    let reached = wait_for_offset(&offset_store, &projection_id, Some(2)).await;
    let observed = recorder.sequences();

    assert_eq!(
        observed,
        vec![2],
        "an unregistered event type must be skipped, not retried forever"
    );
    assert!(
        reached,
        "the offset must reach 2 past the skipped event; observed {observed:?}"
    );
}

/// An event at or below the projector's offset was applied by an earlier
/// incarnation. Accepting it without projecting it is what lets the cursor
/// move past a redelivery; reporting it as a failure would stall the stream.
#[tokio::test(flavor = "multi_thread")]
async fn already_processed_event_is_accepted_without_projecting() {
    let system = ProcessSystem::new().await;
    let event_store = Arc::new(InMemoryEventStore::default());
    let offset_store = Arc::new(InMemoryOffsetStore::default());
    let agg_id = AggregateId::new("duplicate-agg");
    let projection_id = ProjectionId::new("duplicate");

    offset_store
        .save(&OffsetScope::from(&projection_id), 5, None)
        .await
        .expect("seeding the offset must succeed");

    let recorder = Recorder::new();

    let recorder_c = Arc::clone(&recorder);
    let persistence = PersistenceProvider::new(
        CustomStore::builder()
            .eventstore(Arc::clone(&event_store) as Arc<dyn EventStore>)
            .projection_offsets(Arc::clone(&offset_store))
            .build(),
    );
    let proxy = ProjectorProps::new(projection_id.clone(), &persistence, move || {
        RecordingProjector {
            recorder: Arc::clone(&recorder_c),
            fail_sequence: None,
            remaining_failures: budget(0),
        }
    })
    .with_event::<Evt>()
    .delivery_mode(DeliveryMode::AtLeastOnce)
    .catchup_from_aggregate(agg_id.clone())
    .spawn(&system)
    .await;

    let already_processed = LoadedEvent {
        stream_key: agg_id.as_str().to_owned(),
        sequence: 3,
        global_sequence: 3,
        event_type: Evt::EVENT_TYPE,
        payload: Bytes::new(),
        occurred_at: jiff::Timestamp::now(),
    };

    let accepted = proxy.ask(already_processed).await;

    assert!(
        accepted.is_ok(),
        "an event at or below the offset must be accepted so the cursor can move past it, \
         got {accepted:?}"
    );
    assert_eq!(
        recorder.sequences(),
        Vec::<u64>::new(),
        "an already-processed event must not be projected again"
    );
}
