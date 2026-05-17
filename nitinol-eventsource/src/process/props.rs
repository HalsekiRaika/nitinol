use std::sync::Arc;

use nitinol_persistence::AggregateId;
use nitinol_runtime::{Props, ProcessSystem};

use crate::aggregate::{Aggregate, Snapshotable};
use crate::codec::ErasedCodec;
use crate::process::aggregate_process::{AggregateProcess, SnapshotRestoreFn};
use crate::process::persistence::{EventPersistorProxy, SnapshotPersistorProxy};
use crate::process::proxy::AggregateProxy;

pub struct CodecUnset;

pub struct CodecSet<E> {
    pub(crate) codec: Arc<dyn ErasedCodec<E>>,
}

pub struct AggregateProps<A: Aggregate, S = CodecUnset> {
    aggregate_id: AggregateId,
    event_ref: EventPersistorProxy,
    snapshot_ref: Option<SnapshotPersistorProxy>,
    snapshot_restore: Option<SnapshotRestoreFn<A>>,
    state: S,
}

impl<A: Aggregate> AggregateProps<A, CodecUnset> {
    pub fn new(aggregate_id: AggregateId, event_ref: EventPersistorProxy) -> Self {
        Self {
            aggregate_id,
            event_ref,
            snapshot_ref: None,
            snapshot_restore: None,
            state: CodecUnset,
        }
    }

    pub fn with_codec(self, codec: Arc<dyn ErasedCodec<A::Event>>) -> AggregateProps<A, CodecSet<A::Event>> {
        AggregateProps {
            aggregate_id: self.aggregate_id,
            event_ref: self.event_ref,
            snapshot_ref: self.snapshot_ref,
            snapshot_restore: self.snapshot_restore,
            state: CodecSet { codec },
        }
    }
}

impl<A: Aggregate> AggregateProps<A, CodecSet<A::Event>> {
    /// ```compile_fail
    /// # use nitinol_eventsource::AggregateProps;
    /// # use nitinol_persistence::AggregateId;
    /// # use nitinol_eventsource::Aggregate;
    /// # #[derive(Default)]
    /// # struct Dummy;
    /// # impl Aggregate for Dummy { type Event = (); fn apply(&mut self, _: ()) {} }
    /// # async fn _test() {
    /// # let system = nitinol_runtime::ProcessSystem::new().await;
    /// # let event_ref = todo!();
    /// AggregateProps::<Dummy>::new(AggregateId::new("x"), event_ref)
    ///     .spawn(&system)
    ///     .await;
    /// # }
    /// ```
    pub async fn spawn(self, system: &ProcessSystem) -> AggregateProxy<A> {
        let codec = self.state.codec;
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

impl<A: Aggregate + Snapshotable> AggregateProps<A, CodecSet<A::Event>> {
    pub fn with_snapshot_persistor(
        mut self,
        snapshot_ref: SnapshotPersistorProxy,
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
