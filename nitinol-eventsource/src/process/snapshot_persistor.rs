use std::sync::Arc;

use nitinol_persistence::error::SnapshotError;
use nitinol_persistence::provider::SnapshotStoreProvider;
use nitinol_persistence::store::SnapshotStore;
use nitinol_persistence::{AggregateId, PersistedSnapshot};
use nitinol_runtime::process::ProcessProxy;
use nitinol_runtime::process::{Process, ProcessContext, Receive};
use nitinol_runtime::{IdleTimeout, ProcessSystem, Props, SupervisionStrategy};

// Internal messages

pub(crate) struct SaveSnapshot {
    pub(crate) snapshot: PersistedSnapshot,
}

pub(crate) struct LoadLatestSnapshot {
    pub(crate) aggregate_id: AggregateId,
}

// SnapshotPersistor

/// Persistence process that exclusively owns an `Arc<dyn SnapshotStore>`.
///
/// All snapshot operations are serialized through this process's message loop.
pub struct SnapshotPersistor {
    store: Arc<dyn SnapshotStore>,
}

impl Process for SnapshotPersistor {}

impl Receive<SaveSnapshot> for SnapshotPersistor {
    type Response = ();
    type Error = SnapshotError;

    async fn recv(
        &mut self,
        msg: SaveSnapshot,
        _ctx: &mut ProcessContext<Self>,
    ) -> Result<(), SnapshotError> {
        self.store.save(msg.snapshot).await
    }
}

impl Receive<LoadLatestSnapshot> for SnapshotPersistor {
    type Response = Option<PersistedSnapshot>;
    type Error = SnapshotError;

    async fn recv(
        &mut self,
        msg: LoadLatestSnapshot,
        _ctx: &mut ProcessContext<Self>,
    ) -> Result<Option<PersistedSnapshot>, SnapshotError> {
        self.store.load_latest(&msg.aggregate_id).await
    }
}

impl SnapshotPersistor {
    /// Spawn a `SnapshotPersistor` on the given system, over the snapshot store
    /// `persistence` supplies, and return a typed reference.
    ///
    /// - Supervision: `Resume` — the process stays alive after handler errors.
    /// - Idle timeout: `Persistent` — never stopped due to inactivity.
    ///
    /// This is the low-level entry point, for a lifecycle pinned to one
    /// aggregate and started without an
    /// [`EventSourceSystem`](crate::system::EventSourceSystem). A deployment
    /// that has a system takes
    /// [`snapshot_persistor`](crate::system::EventSourceSystem::snapshot_persistor)
    /// from it instead, which starts one process and distributes it; both
    /// resolve the provider's snapshot slot, so neither reaches a store of its
    /// own.
    ///
    /// # Typestate
    ///
    /// A provider that supplies no snapshot store is refused here rather than
    /// at the first save:
    ///
    /// ```compile_fail
    /// # use std::sync::Arc;
    /// # use nitinol_eventsource::SnapshotPersistor;
    /// # use nitinol_persistence::provider::Persistence;
    /// # use nitinol_persistence::store::{EventStore, InMemoryEventStore};
    /// # use nitinol_runtime::ProcessSystem;
    /// # async fn wiring() {
    /// # let ps = ProcessSystem::new().await;
    /// # let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    /// // no `with_snapshot_store` — this provider has no snapshot slot
    /// let persistence = Persistence::new(store);
    /// SnapshotPersistor::spawn(&ps, &persistence).await;
    /// # }
    /// ```
    pub async fn spawn<Pv: SnapshotStoreProvider>(
        system: &ProcessSystem,
        persistence: &Pv,
    ) -> SnapshotPersistorProxy {
        let store = persistence.snapshot_store();
        let props = Props::new(move || SnapshotPersistor {
            store: Arc::clone(&store),
        })
        .with_supervision_strategy(SupervisionStrategy::Resume)
        .with_idle_timeout(IdleTimeout::Persistent);
        system.spawn(props).await.into()
    }
}

impl From<ProcessProxy<SnapshotPersistor>> for SnapshotPersistorProxy {
    fn from(inner: ProcessProxy<SnapshotPersistor>) -> Self {
        Self(inner)
    }
}

// SnapshotPersistorRef

/// Typed, cloneable handle to a `SnapshotPersistor` process.
///
/// Callers save and load snapshots without holding any direct store reference.
#[derive(Clone)]
pub struct SnapshotPersistorProxy(ProcessProxy<SnapshotPersistor>);

impl SnapshotPersistorProxy {
    /// Save a snapshot via the `SnapshotPersistor` process.
    pub async fn save(&self, snapshot: PersistedSnapshot) -> Result<(), SnapshotError> {
        self.0
            .ask(SaveSnapshot { snapshot })
            .await
            .map_err(|e| match e {
                nitinol_runtime::error::AskError::Handler(e) => e,
                _ => SnapshotError::Backend(Box::new(nitinol_runtime::error::SendError)),
            })
    }

    /// Load the latest snapshot via the `SnapshotPersistor` process.
    pub async fn load_latest(
        &self,
        aggregate_id: AggregateId,
    ) -> Result<Option<PersistedSnapshot>, SnapshotError> {
        self.0
            .ask(LoadLatestSnapshot { aggregate_id })
            .await
            .map_err(|e| match e {
                nitinol_runtime::error::AskError::Handler(e) => e,
                _ => SnapshotError::Backend(Box::new(nitinol_runtime::error::SendError)),
            })
    }
}
