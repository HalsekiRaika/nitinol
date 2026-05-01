use std::sync::Arc;

use nitinol_persistence::store::{EventStore, SnapshotStore};
use nitinol_persistence::AggregateId;
use nitinol_runtime::{Props, ProcessSystem};

use crate::aggregate::{Aggregate, Snapshotable};
use crate::process::aggregate_process::{AggregateProcess, SnapshotRestoreFn};
use crate::process::codec::EventCodec;
use crate::process::proxy::AggregateProxy;

/// Builder for spawning an `AggregateProcess<A>` and obtaining an `AggregateProxy<A>`.
///
/// Mandatory: `new(aggregate_id, event_store)` + `with_codec(codec)` + `spawn(system)`.
/// Optional: `with_snapshot_store(store)` (only available when `A: Snapshotable`).
pub struct AggregateProps<A: Aggregate> {
    aggregate_id: AggregateId,
    event_store: Arc<dyn EventStore>,
    snapshot_store: Option<Arc<dyn SnapshotStore>>,
    codec: Option<Arc<dyn EventCodec<A::Event>>>,
    snapshot_restore: Option<SnapshotRestoreFn<A>>,
}

impl<A: Aggregate> AggregateProps<A> {
    pub fn new(aggregate_id: AggregateId, event_store: Arc<dyn EventStore>) -> Self {
        Self {
            aggregate_id,
            event_store,
            snapshot_store: None,
            codec: None,
            snapshot_restore: None,
        }
    }

    /// Set the codec used to encode events for persistence and decode them during replay.
    pub fn with_codec(mut self, codec: Arc<dyn EventCodec<A::Event>>) -> Self {
        self.codec = Some(codec);
        self
    }

    /// Spawn the aggregate process and return a proxy.
    ///
    /// # Panics
    /// Panics if `with_codec` was not called before `spawn`.
    pub async fn spawn(self, system: &ProcessSystem) -> AggregateProxy<A> {
        let codec = self
            .codec
            .expect("AggregateProps::with_codec must be called before spawn");
        let aggregate_id = self.aggregate_id;
        let event_store = self.event_store;
        let snapshot_store = self.snapshot_store;
        let snapshot_restore = self.snapshot_restore;

        let props = Props::new(move || AggregateProcess {
            state: A::default(),
            aggregate_id: aggregate_id.clone(),
            event_store: Arc::clone(&event_store),
            snapshot_store: snapshot_store.clone(),
            codec: Arc::clone(&codec),
            sequence: 0,
            snapshot_restore: snapshot_restore.clone(),
        });

        let proxy = system.spawn(props).await;
        AggregateProxy(proxy)
    }
}

impl<A: Aggregate + Snapshotable> AggregateProps<A> {
    /// Set the snapshot store.  Only available for aggregates that implement
    /// `Snapshotable`.  On_start will attempt to restore from the latest snapshot
    /// before replaying delta events.
    pub fn with_snapshot_store(mut self, store: Arc<dyn SnapshotStore>) -> Self {
        self.snapshot_store = Some(store);
        self.snapshot_restore = Some(Arc::new(|payload: &[u8]| A::restore(payload)));
        self
    }
}
