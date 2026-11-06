use nitinol_contract::Aggregate;
use nitinol_persistence::provider::Store;
use nitinol_persistence::store::{Album, Journal};
use nitinol_persistence::AggregateId;
use nitinol_runtime::ProcessSystem;

use super::{Node, SnapshotSource};
use crate::process::props::resolving_reference;
use crate::process::RecoverOnContingent;
use crate::AggregateProxy;

/// A view of an [`EventSourceSystem`](super::EventSourceSystem) that no longer
/// says, in its type, which slots the deployment's store holds.
///
/// Wiring whose types are fixed before a deployment's store is known — a saga's
/// dispatch mapping names the system it delivers through when the mapping is
/// written — names this one type for every deployment. It is obtained from
/// [`EventSourceSystem::to_contingent`](super::EventSourceSystem::to_contingent)
/// and is the same node, not a copy: the same process system, the same
/// aggregate registry, the same journal and the same snapshot persistor. An
/// identity resolved here reaches the activation the typed system would.
///
/// # What it does not promise
///
/// Whether the deployment holds a snapshot slot is carried as a value, read
/// when the program runs, and [`snapshot_store`][Self::snapshot_store] is the
/// one place it is answered. So everything the typed system offers on the
/// strength of a slot's bound — its snapshot persistor, its props, its
/// supplier — is absent here rather than present and unreliable.
///
/// An aggregate whose [`Recover`](Aggregate::Recover) is
/// [`Latest`](nitinol_contract::Latest) still resolves through this view,
/// because the slot it asks for is read from that value. Where the deployment
/// holds the slot, it recovers from the snapshot this node's persistor keeps,
/// exactly as it would through the typed system. Where the deployment holds
/// none, the activation fails permanently: nothing is started, the stream is
/// not replayed in place of the missing snapshot, and every dispatch through
/// the reference answers [`MissingSnapshotSlot`](crate::error::MissingSnapshotSlot)
/// — an [`AskError::Activation`](crate::AskError::Activation) that
/// [`retryability`](crate::AskError::retryability) reads as permanent.
///
/// That is a wiring mistake, and the typed system still says so when it is
/// compiled: spawning the same aggregate there on the same deployment does not
/// compile.
///
/// # Cloning
///
/// A clone is another name for the same node and carries the same journal and
/// the same answer about the snapshot slot.
#[derive(Clone)]
pub struct ContingentSystem {
    node: Node,
    store: ErasedStore,
}

/// The store a contingent system runs on: the journal, and the snapshot slot
/// as a value.
///
/// It implements no slot trait. Those traits are what wiring compiled against a
/// store reads, and implementing [`SnapshotSlot`](nitinol_persistence::provider::SnapshotSlot)
/// here would hand such wiring a slot it could not tell was missing. The offset
/// slots are not carried at all: what reaches a store through a contingent
/// system is an aggregate, and an aggregate keeps no offset.
#[derive(Clone)]
pub(crate) struct ErasedStore {
    events: Journal,
    snapshots: Option<Album>,
}

impl Store for ErasedStore {
    fn events(&self) -> Journal {
        self.events.clone()
    }

    fn snapshot_slot(&self) -> Option<Album> {
        self.snapshots.clone()
    }
}

impl ContingentSystem {
    /// A view of `node` over the journal and the snapshot slot its typed
    /// system's store answered with.
    pub(super) fn new(node: Node, events: Journal, snapshots: Option<Album>) -> Self {
        Self {
            node,
            store: ErasedStore { events, snapshots },
        }
    }

    /// Returns a reference to the underlying [`ProcessSystem`].
    pub fn process_system(&self) -> &ProcessSystem {
        self.node.process_system()
    }

    /// The journal every stream in this deployment is appended to and replayed
    /// from.
    pub fn event_store(&self) -> Journal {
        self.store.events.clone()
    }

    /// Where aggregate snapshots are kept, or `None` where the deployment
    /// holds no snapshot slot.
    pub fn snapshot_store(&self) -> Option<Album> {
        self.store.snapshots.clone()
    }

    /// A reference to the aggregate `id`, built without awaiting and without
    /// activating anything.
    ///
    /// Resolved on the reference's first dispatch, and again after the
    /// activation it reached dies, through the registry the typed system
    /// resolves through.
    pub fn aggregate_proxy<A>(&self, id: AggregateId) -> AggregateProxy<A>
    where
        A: Aggregate,
        A::Recover: RecoverOnContingent<A>,
    {
        resolving_reference(
            id,
            self.node.resolve_handle(),
            self.event_store(),
            <A::Recover as RecoverOnContingent<A>>::recovery(self),
        )
    }

    /// Resolve the aggregate `id`, activating it if this node has no activation
    /// for it, and return a reference to it.
    ///
    /// Where the aggregate cannot be activated through this view, nothing is
    /// started and the reference answers why on every dispatch.
    pub async fn spawn_aggregate<A>(&self, id: AggregateId) -> AggregateProxy<A>
    where
        A: Aggregate,
        A::Recover: RecoverOnContingent<A>,
    {
        let reference = self.aggregate_proxy::<A>(id);
        reference.activate().await;
        reference
    }

    /// What an activation of a `Recover = Latest` aggregate resolves this
    /// node's one persistor through, once it is running.
    ///
    /// `snapshots` is the album this view's snapshot slot answered with.
    pub(crate) fn snapshot_source(&self, snapshots: Album) -> SnapshotSource {
        self.node.snapshot_source(snapshots)
    }
}
