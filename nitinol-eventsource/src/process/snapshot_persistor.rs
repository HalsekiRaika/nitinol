use nitinol_persistence::error::SnapshotError;
use nitinol_persistence::store::Album;
use nitinol_persistence::{AggregateId, PersistedSnapshot};
use nitinol_runtime::ident::Pid;
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

/// Persistence process that exclusively owns the supplier's [`Album`].
///
/// All snapshot operations are serialized through this process's message loop.
///
/// # One per system, and no way to make a second
///
/// [`EventSourceSystem::snapshot_persistor`](crate::system::EventSourceSystem::snapshot_persistor)
/// is the only way to reach one, and it answers with the single process its
/// node memoises — the same one an activation of a
/// [`Latest`](nitinol_contract::Latest) aggregate recovers through. There is no
/// entry point that starts a persistor over a store a caller named, because the
/// snapshot contract is written for a single consumer serialising its calls:
///
/// ```compile_fail
/// # use nitinol_eventsource::SnapshotPersistor;
/// # use nitinol_persistence::provider::PersistenceProvider;
/// # use nitinol_persistence::store::InMemoryStore;
/// # use nitinol_runtime::ProcessSystem;
/// # async fn wiring() {
/// # let ps = ProcessSystem::new().await;
/// let persistence = PersistenceProvider::new(InMemoryStore::default());
/// // `SnapshotPersistor::spawn` is not public: a system distributes the one
/// // persistor it holds
/// SnapshotPersistor::spawn(&ps, &persistence).await;
/// # }
/// ```
pub struct SnapshotPersistor {
    store: Album,
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
    /// Spawn a `SnapshotPersistor` on `system`, over `store`, and return a typed
    /// reference.
    ///
    /// - Supervision: `Resume` — the process stays alive after handler errors.
    /// - Idle timeout: `Persistent` — never stopped due to inactivity.
    ///
    /// Crate-private, and called from exactly one place: the memoised cell a
    /// system's node holds. The snapshot contract is written for a single
    /// consumer serialising its calls, so a second process over one store would
    /// be two consumers — which is why there is no public form of this and no
    /// way to start a second persistor at all.
    pub(crate) async fn spawn(system: &ProcessSystem, store: Album) -> SnapshotPersistorProxy {
        let props = Props::new(move || SnapshotPersistor {
            store: store.clone(),
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
    /// The process this handle addresses.
    ///
    /// What it is for is telling one persistor from two: the snapshot contract
    /// is written for a single consumer serialising its calls, and a system
    /// handing back a second process over one store would be two consumers —
    /// which a round trip through the store cannot see, because both would
    /// pass it.
    pub fn pid(&self) -> Pid {
        self.0.pid()
    }

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
