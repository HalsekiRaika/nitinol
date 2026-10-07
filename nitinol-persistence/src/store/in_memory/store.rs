use crate::provider::{DeliveryCheckpointSlot, ProjectionCheckpointSlot, SnapshotSlot, Store};
use crate::store::in_memory::{InMemoryCheckpointStore, InMemoryEventStore, InMemorySnapshotStore};

/// The in-memory reference [`Store`]: every slot, over the reference
/// implementations.
///
/// The two checkpoint slots are handles of their own, so a position filed under
/// one consumer's key through one slot is not what the other answers with. That
/// is the stricter half of `PRV-4` and the one a deployment gets by default;
/// the single-table shape — both slots named with one handle — is a
/// [`CustomStore`](crate::provider::CustomStore) a backend with one table is
/// assembled as.
///
/// # A handle, not a region
///
/// [`Default`] constructs a destination holding nothing, and cloning names it:
/// a deployment that keeps the handle it built a system on reads back what that
/// system wrote. That is `PRV-1` as a store owes it.
///
/// Intended for tests and examples; the stores it holds are not for production
/// use.
#[derive(Clone, Default)]
pub struct InMemoryStore {
    events: InMemoryEventStore,
    snapshots: InMemorySnapshotStore,
    projection_checkpoints: InMemoryCheckpointStore,
    delivery_checkpoints: InMemoryCheckpointStore,
}

impl Store for InMemoryStore {
    type Events = InMemoryEventStore;

    fn events(&self) -> InMemoryEventStore {
        self.events.clone()
    }
}

impl SnapshotSlot for InMemoryStore {
    type Snapshots = InMemorySnapshotStore;

    fn snapshots(&self) -> InMemorySnapshotStore {
        self.snapshots.clone()
    }
}

impl ProjectionCheckpointSlot for InMemoryStore {
    type ProjectionCheckpoints = InMemoryCheckpointStore;

    fn projection_checkpoints(&self) -> InMemoryCheckpointStore {
        self.projection_checkpoints.clone()
    }
}

impl DeliveryCheckpointSlot for InMemoryStore {
    type DeliveryCheckpoints = InMemoryCheckpointStore;

    fn delivery_checkpoints(&self) -> InMemoryCheckpointStore {
        self.delivery_checkpoints.clone()
    }
}
