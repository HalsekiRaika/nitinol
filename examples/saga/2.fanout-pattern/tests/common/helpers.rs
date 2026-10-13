//! Wiring shared by the fan-out example's integration tests.
//!
//! Each test binary compiles this module in full but uses only the subset it
//! needs, so per-binary dead code is expected.

#![allow(dead_code)]

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures_util::TryStreamExt;
use tokio::sync::Notify;

use nitinol_eventsource::system::EventSourceSystem;
use nitinol_eventsource::{AggregateProxy, Event};
use nitinol_persistence::error::{AppendError, LoadError};
use nitinol_persistence::provider::{CustomStore, PersistenceProvider, Unprovided};
use nitinol_persistence::store::{
    CheckpointScope, CheckpointStore, EventStore, EventStream, InMemoryCheckpointStore,
};
use nitinol_persistence::{AggregateId, AppendOutcome, AppendingEvent, LoadQuery, LoadedEvent};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{
    SagaDeliveryExt, SagaDeliveryId, SagaDeliveryProxy, SagaDeliveryScope, SagaManagerProxy,
    SagaSystemExt,
};

use saga_fanout_pattern::payroll_run::{PayrollRun, PayrollRunApproved};
use saga_fanout_pattern::payslip::{IsIssued, Payslip};
use saga_fanout_pattern::saga::{FanOutSaga, FanOutStarted};

/// The store this example's deployment assembles: one in-memory journal, named
/// through an `Arc` because the fault-injecting store the crash clauses wrap it
/// in is not a cloneable value of its own, plus the delivery-checkpoint slot a
/// host keeps its progress in.
///
/// Those two slots are exactly what a delivery host needs, and naming the second
/// one is what makes
/// [`SagaDeliveryExt::deliver`](nitinol_saga::SagaDeliveryExt::deliver)
/// exist on the system at all — a deployment that named no store for its hosts'
/// progress would not compile.  The snapshot and projection slots are left
/// unprovided rather than filled with stores nothing here reads.
pub type ExampleStore =
    CustomStore<Arc<dyn EventStore>, Unprovided, Unprovided, InMemoryCheckpointStore>;

/// The system this example's deployment assembles.
pub type ExampleSystem = EventSourceSystem<ExampleStore>;

/// Fan-out width the example is built around: one approval covers 32 employees.
/// Every assertion that counts payslip streams is anchored to it, so an
/// approval that silently narrows the fan-out fails.
pub const EMPLOYEE_COUNT: usize = 32;

/// Number of records the saga's own stream holds for one fan-out: the decision
/// event, and the position of the upstream fact it was derived from — committed
/// by the one append.
///
/// Two, and two whatever the fan-out's width is.  Nothing about the delivery of
/// the 32 commands is persisted: they are re-derived from the decision by
/// `FanOutSaga::dispatch` every time a host reads it, so there is no per-child
/// row to count and no half-written block for a crash to leave behind.
pub const DECISION_BLOCK_LEN: usize = 2;

/// How often a delivery host in these tests asks the store what has been
/// decided.
///
/// Named so that no assertion is tuned against a hidden default, and short
/// enough that a wait below sees many ticks.
pub const DELIVERY_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// The employees an approved payroll run is expected to cover.
///
/// The test owns this derivation rather than the example: the ids travel to the
/// saga inside the fact event, and a test that re-derived them from the saga's
/// output could not detect a fan-out that dropped employees.
pub fn employee_ids() -> Vec<String> {
    (0..EMPLOYEE_COUNT)
        .map(|index| format!("employee-{index:02}"))
        .collect()
}

/// The payslip stream keys a run of `payroll_run` covering `employees` writes.
///
/// The key derivation belongs to the example ([`Payslip::stream_key`]), and the
/// test reads it from there: a second definition of the format would let the
/// two drift apart while both kept passing.  What the test does own is the
/// employee list, which is what still makes a fan-out that dropped an employee
/// observable as a missing stream.
pub fn payslip_keys(payroll_run: &str, employees: &[String]) -> Vec<String> {
    employees
        .iter()
        .map(|employee| Payslip::stream_key(payroll_run, employee))
        .collect()
}

/// Everything one incarnation of the example owns.
///
/// `system` is kept because it is the handle to the `ProcessSystem` the
/// aggregates, the manager and the delivery host run on; dropping the last
/// handle early would take the incarnation down.
///
/// `checkpoints` is kept because a delivery host's progress is the thing these
/// tests observe it by, and it is read as an operator or a second consumer would
/// read it — through the store, not through the host.
pub struct FanOutWorld {
    pub system: ExampleSystem,
    pub payroll_run: AggregateProxy<PayrollRun>,
    pub manager: SagaManagerProxy<FanOutSaga>,
    pub delivery: SagaDeliveryProxy<FanOutSaga>,
    pub checkpoints: InMemoryCheckpointStore,
}

/// Spawn a complete incarnation: a fresh `ProcessSystem`, the payroll run
/// aggregate, the saga manager observing the run's stream, and a delivery host
/// named `delivery_id`.
///
/// One store serves every wiring point — the run aggregate, the payslips, the
/// saga's journal, the manager's subscription and the host's input.
/// `EventStore` is stream-keyed, so the run stream, the 32 payslip streams and
/// the saga's own stream are tenants of the same instance and each owner still
/// reads only its own key.
///
/// Both the journal and the checkpoint store are parameters because the tests
/// hand the same durable state to two successive incarnations — that is what
/// makes "a restarted host resumes from the checkpoint the dead one left" a
/// thing a test can set up, and wrapping the journal in
/// [`UnreachablePayslipStore`] is what makes an interrupted fan-out
/// reproducible.
pub async fn spawn_world(
    run_id: &AggregateId,
    store: Arc<dyn EventStore>,
    checkpoints: InMemoryCheckpointStore,
    delivery_id: &SagaDeliveryId,
) -> FanOutWorld {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps)
        .with_persistence(PersistenceProvider::new(
            CustomStore::builder()
                .eventstore(store)
                .delivery_checkpoints(checkpoints.clone())
                .build(),
        ))
        .build();

    let payroll_run = system.spawn_aggregate::<PayrollRun>(run_id.clone()).await;

    let manager = spawn_manager(&system, run_id).await;
    let delivery = spawn_delivery_host(&system, delivery_id).await;

    FanOutWorld {
        system,
        payroll_run,
        manager,
        delivery,
        checkpoints,
    }
}

/// Spawn (or re-spawn) the saga manager over an existing system.
///
/// The cursor starts at `after: 0` and lives in memory, so a manager spawned a
/// second time over the same store delivers the fact event again.  The saga
/// instance it addresses drops that delivery — the position of the fact is on the
/// instance's own stream, committed by the append that carried the decision — so
/// a second incarnation does not decide the fan-out twice.
///
/// Observing by the run's stream key is also what keeps the manager pointed at
/// the run's own stream while the payslip streams and the saga's journal share
/// the same store.
///
/// The producer ignores the components it is handed: this saga holds nothing,
/// because it sends nothing.
pub async fn spawn_manager(
    system: &ExampleSystem,
    run_id: &AggregateId,
) -> SagaManagerProxy<FanOutSaga> {
    system
        .host::<FanOutSaga>(|_components| FanOutSaga)
        .observing::<PayrollRunApproved>(system.subscription(run_id))
        .spawn()
        .await
}

/// Spawn a delivery host for [`FanOutSaga`] over an existing system, filing its
/// progress under `delivery_id`.
///
/// Nothing else is named: the journal it reads, the slot it records in and the
/// system it delivers through are all the system's.  The identifier is named
/// explicitly rather than left to the default derived from the saga's event
/// type, because it is half of the checkpoint key these tests read — see
/// [`delivery_scope`].
pub async fn spawn_delivery_host(
    system: &ExampleSystem,
    delivery_id: &SagaDeliveryId,
) -> SagaDeliveryProxy<FanOutSaga> {
    system
        .deliver::<FanOutSaga>()
        .with_delivery_id(delivery_id.clone())
        .with_poll_interval(DELIVERY_POLL_INTERVAL)
        .spawn()
        .await
}

// The delivery host's progress

/// The checkpoint key the only partition of the host named `delivery_id`
/// records under.
///
/// Progress is per partition, not per host; these tests leave the count at the
/// default of one, so partition zero is the whole of it.
pub fn delivery_scope(delivery_id: &SagaDeliveryId) -> SagaDeliveryScope {
    SagaDeliveryScope::new(delivery_id, 0)
}

/// What the store holds for `scope`, as an operator would read it.
///
/// `None` is "this host has never finished a record", which is not the same as
/// "it is at zero": the distinction is part of the checkpoint contract, and it is
/// what lets a test say "the interrupted host recorded no progress at all".
pub async fn delivered_through(
    checkpoints: &InMemoryCheckpointStore,
    scope: &dyn CheckpointScope,
) -> Option<u64> {
    checkpoints
        .load(scope)
        .await
        .expect("loading an in-memory checkpoint must succeed")
}

/// Poll until `scope` holds at least `want`, then answer what it holds.
///
/// A host records the position of a record only once *every* delivery that
/// record implied has landed, so this is the observable point that says "the
/// whole fan-out was carried" — not a guessed settle window.
pub async fn wait_for_delivery_through(
    checkpoints: &InMemoryCheckpointStore,
    scope: &dyn CheckpointScope,
    want: u64,
    timeout: Duration,
) -> u64 {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Some(held) = delivered_through(checkpoints, scope).await {
            if held >= want {
                return held;
            }
        }
        if std::time::Instant::now() >= deadline {
            panic!(
                "timed out waiting for delivery progress to reach {want} on {} (held {:?})",
                scope.checkpoint_key(),
                delivered_through(checkpoints, scope).await
            );
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Give a host a window in which it would have made progress if it were going
/// to, so a test can assert that it did not.
///
/// Sized against [`DELIVERY_POLL_INTERVAL`] rather than a bare constant: it has
/// to cover several ticks of the loop under test.
pub async fn settle() {
    tokio::time::sleep(DELIVERY_POLL_INTERVAL * 20).await;
}

// Store observation

pub async fn load_stream(store: &Arc<dyn EventStore>, key: &str) -> Vec<LoadedEvent> {
    store
        .load(LoadQuery::by_stream(key))
        .await
        .expect("load stream must succeed")
        .try_collect()
        .await
        .expect("collect stream must succeed")
}

/// Positional identity of every record in a stream: stream sequence, the
/// store-wide `global_sequence` assigned at commit, and the stored payload.
///
/// Comparing two fingerprints of the same stream taken before and after a
/// redelivery distinguishes "the first write was left intact" from "the record
/// was rewritten with equal content".
pub type StreamFingerprint = Vec<(u64, u64, Vec<u8>)>;

pub async fn fingerprint(store: &Arc<dyn EventStore>, key: &str) -> StreamFingerprint {
    load_stream(store, key)
        .await
        .into_iter()
        .map(|event| {
            (
                event.sequence,
                event.global_sequence,
                event.payload.to_vec(),
            )
        })
        .collect()
}

pub async fn fingerprints(store: &Arc<dyn EventStore>, keys: &[String]) -> Vec<StreamFingerprint> {
    let mut out = Vec::with_capacity(keys.len());
    for key in keys {
        out.push(fingerprint(store, key).await);
    }
    out
}

/// The subset of `keys` whose stream already holds at least one record.
pub async fn issued(store: &Arc<dyn EventStore>, keys: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for key in keys {
        if !load_stream(store, key).await.is_empty() {
            out.push(key.clone());
        }
    }
    out
}

/// Poll the store until exactly `expected` of `keys` exist.
///
/// A payslip's own stream is the only place its issue becomes observable, so
/// polling it — rather than sleeping for a guessed duration — is what makes the
/// positive half of both tests deterministic.
pub async fn wait_for_issued(
    store: &Arc<dyn EventStore>,
    keys: &[String],
    expected: usize,
    timeout: Duration,
) -> Vec<String> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let found = issued(store, keys).await;
        if found.len() >= expected {
            return found;
        }
        if std::time::Instant::now() >= deadline {
            panic!(
                "timed out waiting for {expected} issued payslip streams (had {}: {:?})",
                found.len(),
                found
            );
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Poll the stream `key` until it holds at least `expected` records.
pub async fn wait_for_records(
    store: &Arc<dyn EventStore>,
    key: &str,
    expected: usize,
    timeout: Duration,
) -> Vec<LoadedEvent> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let events = load_stream(store, key).await;
        if events.len() >= expected {
            return events;
        }
        if std::time::Instant::now() >= deadline {
            panic!(
                "timed out waiting for {expected} records on {key} (had {})",
                events.len()
            );
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// The decisions the saga's own stream records, in stream order.
pub fn decisions(events: &[LoadedEvent]) -> Vec<&LoadedEvent> {
    events
        .iter()
        .filter(|event| event.event_type == FanOutStarted::EVENT_TYPE)
        .collect()
}

/// The one decision on the saga's own stream, and the store-wide position it was
/// committed at.
///
/// That position is what a delivery host's checkpoint is compared against: a
/// host that has recorded it has finished every delivery the decision implied.
pub async fn decision_position(store: &Arc<dyn EventStore>, saga_key: &str) -> u64 {
    let stream = load_stream(store, saga_key).await;
    let decisions = decisions(&stream);
    assert_eq!(
        decisions.len(),
        1,
        "the fan-out must have recorded exactly one decision (saga stream holds {} records)",
        stream.len()
    );
    decisions[0].global_sequence
}

/// Round-trip a query through every payslip process's mailbox.
///
/// A delivery is an `ask`, so it has already been answered by the time the host
/// records progress; this is the same round trip taken from the test's side, and
/// it is what reads a payslip's state rather than its stream.
pub async fn drain_payslips(system: &ExampleSystem, keys: &[String]) -> Vec<bool> {
    let mut out = Vec::with_capacity(keys.len());
    for key in keys {
        let proxy: AggregateProxy<Payslip> =
            system.aggregate_proxy::<Payslip>(AggregateId::new(key));
        out.push(
            proxy
                .exec(IsIssued)
                .await
                .expect("exec(IsIssued) must succeed"),
        );
    }
    out
}

// Fault injection

/// `EventStore` decorator that makes a chosen set of payslip streams
/// permanently unwritable, with no timing dependency.
///
/// This is how an incarnation that cannot finish its fan-out is modelled.  Every
/// append to a stream named in `unreachable` is refused, so those payslips go
/// missing the way the ones a dying incarnation never reached do, and the
/// delivery host hits one it cannot get carried out.  The refusal is never
/// lifted: the host wrapped in this store can therefore *never* issue them, which
/// is what makes "the second incarnation completed the fan-out" unambiguous —
/// those 24 streams could have been written by nothing else.
///
/// Refusing by name rather than allowing by name is what lets one store back the
/// whole incarnation: the run's own stream and the saga's own journal keep
/// working, so the decision that drives the fan-out is recorded exactly as in a
/// healthy run.
///
/// `load` always delegates: a later incarnation has to be able to read the
/// journal this one wrote, and a payslip whose issue was refused must still be
/// able to replay (and find itself empty).
pub struct UnreachablePayslipStore {
    inner: Arc<dyn EventStore>,
    /// Payslip streams this incarnation can never write.
    unreachable: HashSet<String>,
    refused: Mutex<HashSet<String>>,
    refused_changed: Notify,
}

impl UnreachablePayslipStore {
    pub fn new(inner: Arc<dyn EventStore>, unreachable: HashSet<String>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            unreachable,
            refused: Mutex::new(HashSet::new()),
            refused_changed: Notify::new(),
        })
    }

    /// The payslip streams that have had an append refused.
    pub fn refused(&self) -> HashSet<String> {
        self.refused
            .lock()
            .expect("refused mutex is never poisoned: no holder panics while the guard is alive")
            .clone()
    }

    /// Wait until at least one append has been refused, and answer every stream
    /// that has had one.
    ///
    /// Distinct streams, not attempts: the caller wants "the fan-out reached a
    /// payslip it cannot write", and counting attempts would also count the
    /// retries of the same one — which a held host makes forever.
    pub async fn wait_until_refused(&self, timeout: Duration) -> HashSet<String> {
        tokio::time::timeout(timeout, async {
            loop {
                let changed = self.refused_changed.notified();
                let refused = self.refused();
                if !refused.is_empty() {
                    return refused;
                }
                changed.await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for a refused payslip append"))
    }
}

#[async_trait]
impl EventStore for UnreachablePayslipStore {
    async fn append(
        &self,
        key: &str,
        events: Vec<AppendingEvent>,
    ) -> Result<AppendOutcome, AppendError> {
        if !self.unreachable.contains(key) {
            return self.inner.append(key, events).await;
        }
        self.refused
            .lock()
            .expect("refused mutex is never poisoned: no holder panics while the guard is alive")
            .insert(key.to_owned());
        self.refused_changed.notify_one();
        Err(AppendError::Backend(Box::new(std::io::Error::other(
            format!("the incarnation issuing payslips cannot reach {key}"),
        ))))
    }

    async fn load(&self, query: LoadQuery) -> Result<EventStream<'_>, LoadError> {
        self.inner.load(query).await
    }
}
