use std::sync::Arc;

use nitinol_persistence::AggregateId;
use nitinol_runtime::{Props, ProcessSystem};

use crate::aggregate::{Aggregate, Snapshotable};
use crate::codec::ErasedCodec;
use crate::process::aggregate_process::{AggregateProcess, SnapshotRestoreFn};
use crate::process::persistence::{EventPersistorRef, SnapshotPersistorRef};
use crate::process::proxy::AggregateProxy;

/// Builder for spawning an `AggregateProcess<A>` and obtaining an `AggregateProxy<A>`.
///
/// Mandatory: `new(aggregate_id, event_ref)` + `with_codec(codec)` + `spawn(system)`.
/// Optional: `with_snapshot_persistor(snapshot_ref, snapshot_codec)` (only available when `A: Snapshotable`).
pub struct AggregateProps<A: Aggregate> {
    aggregate_id: AggregateId,
    event_ref: EventPersistorRef,
    snapshot_ref: Option<SnapshotPersistorRef>,
    codec: Option<Arc<dyn ErasedCodec<A::Event>>>,
    snapshot_restore: Option<SnapshotRestoreFn<A>>,
}

impl<A: Aggregate> AggregateProps<A> {
    pub fn new(aggregate_id: AggregateId, event_ref: EventPersistorRef) -> Self {
        Self {
            aggregate_id,
            event_ref,
            snapshot_ref: None,
            codec: None,
            snapshot_restore: None,
        }
    }

    /// Set the codec used to encode events for persistence and decode them during replay.
    pub fn with_codec(mut self, codec: Arc<dyn ErasedCodec<A::Event>>) -> Self {
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
        let event_ref = self.event_ref;
        let snapshot_ref = self.snapshot_ref;
        let snapshot_restore = self.snapshot_restore;

        let props = Props::new(move || AggregateProcess {
            state: A::default(),
            aggregate_id: aggregate_id.clone(),
            event_ref: event_ref.clone(),
            snapshot_ref: snapshot_ref.clone(),
            codec: Arc::clone(&codec),
            sequence: 0,
            snapshot_restore: snapshot_restore.clone(),
        });

        let proxy = system.spawn(props).await;
        AggregateProxy(proxy)
    }
}

impl<A: Aggregate + Snapshotable> AggregateProps<A> {
    /// Set the snapshot persistor and the codec used to encode/decode snapshots.
    ///
    /// The codec encodes `A::Snapshot` (the pure domain value) to bytes for storage,
    /// and decodes bytes back to `A::Snapshot` before calling `A::restore`.
    ///
    /// On start, the process attempts to restore from the latest snapshot before
    /// replaying delta events.
    pub fn with_snapshot_persistor(
        mut self,
        snapshot_ref: SnapshotPersistorRef,
        snapshot_codec: Arc<dyn ErasedCodec<A::Snapshot>>,
    ) -> Self {
        self.snapshot_ref = Some(snapshot_ref);
        self.snapshot_restore = Some(Arc::new(move |payload: &[u8]| {
            let snapshot = snapshot_codec.decode(payload)?;
            Ok(A::restore(snapshot))
        }));
        self
    }
}
