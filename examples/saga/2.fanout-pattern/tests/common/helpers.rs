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
use nitinol_eventsource::{AggregateProxy, Event, SequenceCursor};
use nitinol_persistence::error::{AppendError, LoadError};
use nitinol_persistence::store::{EventStore, EventStream};
use nitinol_persistence::{AggregateId, AppendOutcome, AppendingEvent, LoadQuery, LoadedEvent};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{SagaManagerProps, SagaManagerProxy};

use saga_fanout_pattern::batch::{Batch, BatchDecomposed};
use saga_fanout_pattern::codec::JsonCodec;
use saga_fanout_pattern::item::{IsCreated, Item};
use saga_fanout_pattern::router::ItemRegistry;
use saga_fanout_pattern::saga::{FanOutSaga, FanOutStarted};

/// Fan-out width the example is built around: one fact event carries 32 child
/// creations. Every assertion that counts child streams is anchored to it, so a
/// decomposition that silently narrows the fan-out fails.
pub const ITEM_COUNT: usize = 32;

/// Number of records the saga's own stream must hold for one decision: the
/// decision event plus one outbox marker per dispatched tell.
pub const DECISION_BLOCK_LEN: usize = 1 + ITEM_COUNT;

/// The child stream keys a decomposition of `batch` is expected to name.
///
/// The test owns this derivation rather than the example: the ids travel to the
/// saga inside the fact event, and a test that re-derived them from the saga's
/// output could not detect a fan-out that dropped children.
pub fn item_ids(batch: &str) -> Vec<String> {
    (0..ITEM_COUNT)
        .map(|index| format!("{batch}-item-{index:02}"))
        .collect()
}

/// Everything one incarnation of the example owns.
///
/// `system` is kept because it owns the `ProcessSystem` the aggregates and the
/// manager run on; dropping it early would take the incarnation down.
pub struct FanOutWorld {
    pub system: Arc<EventSourceSystem<JsonCodec>>,
    pub registry: Arc<ItemRegistry>,
    pub batch: AggregateProxy<Batch>,
    pub manager: SagaManagerProxy<FanOutSaga>,
}

/// Spawn a complete incarnation: a fresh `ProcessSystem`, the batch aggregate,
/// the child registry, and the saga manager subscribed to the batch stream.
///
/// The three stores are parameters because the crash/replay test hands the same
/// batch and saga stores to two successive incarnations while swapping the item
/// store, which is what makes a partially completed fan-out reproducible.
pub async fn spawn_world(
    batch_id: &AggregateId,
    batch_store: Arc<dyn EventStore>,
    item_store: Arc<dyn EventStore>,
    saga_store: Arc<dyn EventStore>,
) -> FanOutWorld {
    let ps = ProcessSystem::new().await;
    let system = Arc::new(EventSourceSystem::new(ps).with_codec::<JsonCodec>().build());

    let registry = Arc::new(ItemRegistry::new(Arc::clone(&system), item_store));

    let batch = system
        .spawn_aggregate::<Batch>(batch_id.clone(), Arc::clone(&batch_store))
        .await;

    let manager = spawn_manager(&system, &registry, batch_id, batch_store, saga_store).await;

    FanOutWorld {
        system,
        registry,
        batch,
        manager,
    }
}

/// Spawn (or re-spawn) the saga manager over an existing system and registry.
///
/// The cursor starts at `after: 0` and lives in memory, so a manager spawned a
/// second time over the same batch store replays the fact event — the
/// at-least-once redelivery the pattern has to absorb.
pub async fn spawn_manager(
    system: &Arc<EventSourceSystem<JsonCodec>>,
    registry: &Arc<ItemRegistry>,
    batch_id: &AggregateId,
    batch_store: Arc<dyn EventStore>,
    saga_store: Arc<dyn EventStore>,
) -> SagaManagerProxy<FanOutSaga> {
    let registry_for_producer = Arc::clone(registry);
    let registry_for_factory = Arc::clone(registry);

    SagaManagerProps::<FanOutSaga>::new(saga_store, move || {
        FanOutSaga::new(Arc::clone(&registry_for_producer))
    })
    .with_codec(system.codec::<FanOutStarted>())
    .with_subscription(
        batch_store,
        system.codec::<BatchDecomposed>(),
        SequenceCursor::Stream {
            key: batch_id.as_str().to_owned(),
            after: 0,
        },
    )
    .with_crash_restart_factory(move |payload: &[u8]| {
        FanOutSaga::crash_restart_intent(&registry_for_factory, payload)
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
pub async fn created(store: &Arc<dyn EventStore>, keys: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for key in keys {
        if !load_stream(store, key).await.is_empty() {
            out.push(key.clone());
        }
    }
    out
}

/// Poll the item store until exactly `expected` of `keys` exist.
///
/// The store is the only place a child creation becomes observable, so polling
/// it — rather than sleeping for a guessed duration — is what makes the
/// positive half of both tests deterministic.
pub async fn wait_for_created(
    store: &Arc<dyn EventStore>,
    keys: &[String],
    expected: usize,
    timeout: Duration,
) -> Vec<String> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let found = created(store, keys).await;
        if found.len() >= expected {
            return found;
        }
        if std::time::Instant::now() >= deadline {
            panic!(
                "timed out waiting for {expected} created item streams (had {}: {:?})",
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
    saga_store: &Arc<dyn EventStore>,
    saga_key: &str,
    expected: usize,
    timeout: Duration,
) -> Vec<LoadedEvent> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let events = load_stream(saga_store, saga_key).await;
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

/// Round-trip a query through every child process's mailbox.
///
/// `AggregateProxy::tell` returns once the command is queued, so a later `exec`
/// on the same proxy cannot be served before that command has been handled.
/// Draining the mailbox this way is what stops "no duplicate was written" from
/// passing merely because the redelivered command had not been processed yet.
pub async fn drain_children(registry: &Arc<ItemRegistry>, keys: &[String]) -> Vec<bool> {
    let mut out = Vec::with_capacity(keys.len());
    for key in keys {
        let proxy: AggregateProxy<Item> = registry.resolve(key).await;
        out.push(
            proxy
                .exec(IsCreated)
                .await
                .expect("exec(IsCreated) must succeed"),
        );
    }
    out
}

// Fault injection

/// `EventStore` decorator that refuses `append` for every stream outside
/// `allowed`, so a fan-out can be stopped part-way with no timing dependency.
///
/// `load` always delegates: a child whose creation was refused must still be
/// able to replay (and find itself empty) in a later incarnation.
pub struct PartialFailureItemStore {
    inner: Arc<dyn EventStore>,
    allowed: HashSet<String>,
    refused: Mutex<HashSet<String>>,
    refused_changed: Notify,
}

impl PartialFailureItemStore {
    pub fn new(inner: Arc<dyn EventStore>, allowed: HashSet<String>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            allowed,
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
    /// every child it was going to fail on", and counting attempts would also
    /// count a retry of the same child.
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
                "timed out waiting for {expected} refused child appends (had {:?})",
                self.refused()
            )
        })
    }
}

#[async_trait]
impl EventStore for PartialFailureItemStore {
    async fn append(
        &self,
        key: &str,
        events: Vec<AppendingEvent>,
    ) -> Result<AppendOutcome, AppendError> {
        if self.allowed.contains(key) {
            return self.inner.append(key, events).await;
        }
        self.refused
            .lock()
            .expect("refused mutex is never poisoned: no holder panics while the guard is alive")
            .insert(key.to_owned());
        self.refused_changed.notify_one();
        Err(AppendError::Backend(Box::new(std::io::Error::other(
            format!("fan-out interrupted before {key} was created"),
        ))))
    }

    async fn load(&self, query: LoadQuery) -> Result<EventStream<'_>, LoadError> {
        self.inner.load(query).await
    }
}
