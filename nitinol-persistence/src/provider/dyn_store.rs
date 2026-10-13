//! A store handle whose backend type is no longer in the signature.

use std::sync::Arc;

use crate::provider::Store;
use crate::store::EventStore;

/// A [`Store`] handle with the backend's type erased, answering for the journal
/// alone.
///
/// What this is for is wiring whose types are fixed before a deployment's store
/// is known: a saga's dispatch mapping names one system type when the mapping
/// is written, long before anyone assembles a store. The erased handle names
/// one.
///
/// It reaches the store it was built from rather than a copy of its shape, and
/// its own clone reaches the same — which is `PRV-1` carried through the
/// erasure, and what the wiring holding it depends on.
///
/// # No slot is reachable through it
///
/// There is deliberately no [`SnapshotSlot`](crate::provider::SnapshotSlot) or
/// offset-slot implementation here, and no accessor that could answer with
/// one. A slot a deployment did not supply has to be refused while the wiring
/// is compiled, and an erased handle that offered slots would be the one place
/// where it could only be refused at the first save instead.
#[derive(Clone)]
pub struct DynStore {
    events: Arc<dyn EventStore>,
}

impl DynStore {
    /// Erase `store`'s type, keeping the journal it holds.
    pub fn new<S: Store>(store: S) -> Self {
        Self {
            events: Arc::new(store.events()),
        }
    }
}

impl Store for DynStore {
    type Events = Arc<dyn EventStore>;

    fn events(&self) -> Arc<dyn EventStore> {
        Arc::clone(&self.events)
    }
}
