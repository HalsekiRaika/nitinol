use nitinol_contract::Aggregate;
use nitinol_persistence::provider::Store;
use nitinol_persistence::store::{Album, Journal};
use nitinol_persistence::AggregateId;
use nitinol_runtime::ProcessSystem;

use super::{Node, SnapshotSource};
use crate::process::props::resolving_reference;
use crate::process::RecoverOnContingent;
use crate::AggregateProxy;

/// A view of an [`EventSourceSystem`](super::EventSourceSystem) that does not
/// say, in its type, which slots the deployment's store holds.
///
/// - For wiring whose types are fixed before the store is known, such as a
///   saga's dispatch mapping. Obtained from
///   [`to_contingent`](super::EventSourceSystem::to_contingent), it is the same
///   node as the typed system, and a clone carries the same journal and the
///   same snapshot slot.
/// - The snapshot slot is a value ([`snapshot_store`][Self::snapshot_store]),
///   so the typed system's snapshot persistor, props and supplier are absent.
/// - An aggregate whose [`Recover`](Aggregate::Recover) is
///   [`Latest`](nitinol_contract::Latest) recovers from the snapshot where the
///   slot exists. Where it does not, nothing is started, the stream is not
///   replayed instead, and every dispatch answers
///   [`MissingSnapshotSlot`](crate::error::MissingSnapshotSlot), an
///   [`AskError::Activation`](crate::AskError::Activation) that
///   [`retryability`](crate::AskError::retryability) reads as permanent.
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
