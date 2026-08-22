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

use nitinol_eventsource::system::{EventSourceSystem, StoreSet};
use nitinol_eventsource::{AggregateProxy, Event};
use nitinol_persistence::error::{AppendError, LoadError};
use nitinol_persistence::store::{EventStore, EventStream};
use nitinol_persistence::{AggregateId, AppendOutcome, AppendingEvent, LoadQuery, LoadedEvent};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{SagaDefaultStoreExt, SagaManagerProxy};

use saga_fanout_pattern::codec::JsonCodec;
use saga_fanout_pattern::payroll_run::PayrollRun;
use saga_fanout_pattern::payslip::{IsIssued, Payslip};
use saga_fanout_pattern::saga::{FanOutSaga, FanOutStarted};

/// Fan-out width the example is built around: one approval covers 32 employees.
/// Every assertion that counts payslip streams is anchored to it, so an
/// approval that silently narrows the fan-out fails.
pub const EMPLOYEE_COUNT: usize = 32;

/// Number of records the saga's own stream must hold for one decision: the
/// decision event plus one outbox marker per dispatched tell.
pub const DECISION_BLOCK_LEN: usize = 1 + EMPLOYEE_COUNT;

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
/// aggregates and the manager run on; dropping the last handle early would take
/// the incarnation down.
pub struct FanOutWorld {
    pub system: EventSourceSystem<JsonCodec, StoreSet>,
    pub payroll_run: AggregateProxy<PayrollRun>,
    pub manager: SagaManagerProxy<FanOutSaga>,
}

/// Spawn a complete incarnation: a fresh `ProcessSystem`, the payroll run
/// aggregate, and the saga manager subscribed to the run's stream.
///
/// One store serves every wiring point — the run aggregate, the payslips, the
/// saga's journal and the manager's subscription.  `EventStore` is
/// stream-keyed, so the run stream, the 32 payslip streams and the saga's own
/// stream are tenants of the same instance and each owner still reads only its
/// own key.
///
/// The store is a parameter because the crash/replay test hands the same durable
/// state to two successive incarnations, wrapping the first one's in a
/// fault-injecting decorator — which is what makes a partially completed fan-out
/// reproducible.
pub async fn spawn_world(run_id: &AggregateId, store: Arc<dyn EventStore>) -> FanOutWorld {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps)
        .with_codec::<JsonCodec>()
        .with_event_store(store)
        .build();

    let payroll_run = system.spawn_aggregate::<PayrollRun>(run_id.clone()).await;

    let manager = spawn_manager(&system, run_id).await;

    FanOutWorld {
        system,
        payroll_run,
        manager,
    }
}

/// Spawn (or re-spawn) the saga manager over an existing system.
///
/// The cursor starts at `after: 0` and lives in memory, so a manager spawned a
/// second time over the same store replays the fact event — the at-least-once
/// redelivery the pattern has to absorb.
///
/// Subscribing by the run's stream key is also what keeps the manager pointed at
/// the run's own stream while the payslip streams and the saga's journal share
/// the same store.
pub async fn spawn_manager(
    system: &EventSourceSystem<JsonCodec, StoreSet>,
    run_id: &AggregateId,
) -> SagaManagerProxy<FanOutSaga> {
    let system_for_producer = system.clone();
    let system_for_factory = system.clone();

    system
        .saga_manager_props(system.subscription(run_id), move || {
            FanOutSaga::new(system_for_producer.clone())
        })
        .with_crash_restart_factory(move |payload: &[u8]| {
            FanOutSaga::crash_restart_intent(&system_for_factory, payload)
        })
        .spawn(system.process_system())
        .await
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

/// Poll the saga's own stream until it holds at least `expected` records of
/// `FanOutStarted` — one per decision the saga committed.
pub async fn wait_for_decisions(
    store: &Arc<dyn EventStore>,
    saga_key: &str,
    expected: usize,
    timeout: Duration,
) -> Vec<LoadedEvent> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let events = load_stream(store, saga_key).await;
        let decisions = events
            .iter()
            .filter(|event| event.event_type == FanOutStarted::EVENT_TYPE)
            .count();
        if decisions >= expected {
            return events;
        }
        if std::time::Instant::now() >= deadline {
            panic!(
                "timed out waiting for {expected} FanOutStarted records on {saga_key} \
                 (had {decisions} among {} records)",
                events.len()
            );
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Round-trip a query through every payslip process's mailbox.
///
/// `AggregateProxy::tell` returns once the command is queued, so a later `exec`
/// through a reference to the same aggregate cannot be served before that
/// command has been handled — both dispatches resolve to the one activation.
/// Draining the mailbox this way is what stops "no duplicate was written" from
/// passing merely because the redelivered command had not been processed yet.
pub async fn drain_payslips(
    system: &EventSourceSystem<JsonCodec, StoreSet>,
    keys: &[String],
) -> Vec<bool> {
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

/// `EventStore` decorator that refuses `append` for the streams named in
/// `refuses`, so a fan-out can be stopped part-way with no timing dependency.
///
/// Refusing by name rather than allowing by name is what lets one store back the
/// whole incarnation: the run's own stream and the saga's journal keep working,
/// so the decision that drives the fan-out is still recorded, and only the
/// payslips named here go missing.
///
/// `load` always delegates: a payslip whose issue was refused must still be able
/// to replay (and find itself empty) in a later incarnation.
pub struct PartialFailurePayslipStore {
    inner: Arc<dyn EventStore>,
    refuses: HashSet<String>,
    refused: Mutex<HashSet<String>>,
    refused_changed: Notify,
}

impl PartialFailurePayslipStore {
    pub fn new(inner: Arc<dyn EventStore>, refuses: HashSet<String>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            refuses,
            refused: Mutex::new(HashSet::new()),
            refused_changed: Notify::new(),
        })
    }

    pub fn refused(&self) -> HashSet<String> {
        self.refused
            .lock()
            .expect("refused mutex is never poisoned: no holder panics while the guard is alive")
            .clone()
    }

    /// Wait until `expected` distinct streams have had an append refused.
    ///
    /// Distinct streams, not attempts: the caller wants "the fan-out reached
    /// every payslip it was going to fail on", and counting attempts would also
    /// count a retry of the same payslip.
    pub async fn wait_until_refused(&self, expected: usize, timeout: Duration) -> HashSet<String> {
        tokio::time::timeout(timeout, async {
            loop {
                let changed = self.refused_changed.notified();
                let refused = self.refused();
                if refused.len() >= expected {
                    return refused;
                }
                changed.await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "timed out waiting for {expected} refused payslip appends (had {:?})",
                self.refused()
            )
        })
    }
}

#[async_trait]
impl EventStore for PartialFailurePayslipStore {
    async fn append(
        &self,
        key: &str,
        events: Vec<AppendingEvent>,
    ) -> Result<AppendOutcome, AppendError> {
        if !self.refuses.contains(key) {
            return self.inner.append(key, events).await;
        }
        self.refused
            .lock()
            .expect("refused mutex is never poisoned: no holder panics while the guard is alive")
            .insert(key.to_owned());
        self.refused_changed.notify_one();
        Err(AppendError::Backend(Box::new(std::io::Error::other(
            format!("fan-out interrupted before {key} was issued"),
        ))))
    }

    async fn load(&self, query: LoadQuery) -> Result<EventStream<'_>, LoadError> {
        self.inner.load(query).await
    }
}
