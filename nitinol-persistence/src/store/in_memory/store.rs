use crate::provider::{ProjectionOffsetSlot, SagaOffsetSlot, SnapshotSlot, Store};
use crate::store::in_memory::{InMemoryEventStore, InMemoryOffsetStore, InMemorySnapshotStore};
use crate::store::{Album, Journal};

/// The in-memory reference [`Store`]: every slot, over the reference
/// implementations.
///
/// The two offset slots are handles of their own, so a position filed under
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
#[derive(Clone)]
pub struct InMemoryStore {
    events: Journal,
    snapshots: Album,
    projection_offsets: InMemoryOffsetStore,
    saga_offsets: InMemoryOffsetStore,
}

/// The two erased slots are erased here, once, so cloning the store clones the
/// one handle each was built over rather than erasing a fresh backend per call.
impl Default for InMemoryStore {
    fn default() -> Self {
        Self {
            events: Journal::from(InMemoryEventStore::default()),
            snapshots: Album::from(InMemorySnapshotStore::default()),
            projection_offsets: InMemoryOffsetStore::default(),
            saga_offsets: InMemoryOffsetStore::default(),
        }
    }
}

impl Store for InMemoryStore {
    fn events(&self) -> Journal {
        self.events.clone()
    }
}

impl SnapshotSlot for InMemoryStore {
    fn snapshots(&self) -> Album {
        self.snapshots.clone()
    }
}

impl ProjectionOffsetSlot for InMemoryStore {
    type ProjectionOffsets = InMemoryOffsetStore;

    fn projection_offsets(&self) -> InMemoryOffsetStore {
        self.projection_offsets.clone()
    }
}

impl SagaOffsetSlot for InMemoryStore {
    type SagaOffsets = InMemoryOffsetStore;

    fn saga_offsets(&self) -> InMemoryOffsetStore {
        self.saga_offsets.clone()
    }
}
