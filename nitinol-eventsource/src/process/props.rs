use std::sync::Arc;

use nitinol_persistence::store::EventStore;
use nitinol_persistence::AggregateId;
use nitinol_runtime::{ProcessSystem, Props};

use crate::aggregate::{Aggregate, Snapshotable};
use crate::codec::ErasedCodec;
use crate::process::aggregate_process::{AggregateProcess, SnapshotRestoreFn};
use crate::process::proxy::AggregateProxy;
use crate::process::snapshot_persistor::SnapshotPersistorProxy;

pub struct CodecUnset;

pub struct CodecSet<E> {
    pub(crate) codec: Arc<dyn ErasedCodec<E>>,
}

pub struct AggregateProps<A: Aggregate, S = CodecUnset> {
    aggregate_id: AggregateId,
    store: Arc<dyn EventStore>,
    snapshot_ref: Option<SnapshotPersistorProxy>,
    snapshot_restore: Option<SnapshotRestoreFn<A>>,
    state: S,
}

impl<A: Aggregate> AggregateProps<A, CodecUnset> {
    pub fn new(aggregate_id: AggregateId, store: Arc<dyn EventStore>) -> Self {
        Self {
            aggregate_id,
            store,
            snapshot_ref: None,
            snapshot_restore: None,
            state: CodecUnset,
        }
    }

    pub fn with_codec(
        self,
        codec: Arc<dyn ErasedCodec<A::Event>>,
    ) -> AggregateProps<A, CodecSet<A::Event>> {
        AggregateProps {
            aggregate_id: self.aggregate_id,
            store: self.store,
            snapshot_ref: self.snapshot_ref,
            snapshot_restore: self.snapshot_restore,
            state: CodecSet { codec },
        }
    }
}

impl<A: Aggregate> AggregateProps<A, CodecSet<A::Event>> {
    /// Spawn this aggregate process.
    ///
    /// [`with_codec`][AggregateProps::with_codec] must be called before `spawn`.
    /// Calling `spawn` on a builder in [`CodecUnset`] state is a **compile error**:
    ///
    /// ```compile_fail
    /// # use std::sync::Arc;
    /// # use nitinol_eventsource::{Aggregate, AggregateProps, Event};
    /// # use nitinol_persistence::{AggregateId, EventType, Family, TypeName};
    /// # use nitinol_persistence::store::{EventStore, InMemoryEventStore};
    /// # use nitinol_runtime::ProcessSystem;
    /// # #[derive(Default)] struct MyAgg;
    /// # #[derive(Clone, PartialEq, Debug)] struct MyEv;
    /// # impl Event for MyEv { const EVENT_TYPE: EventType = EventType::new(Family::new(""), TypeName::new("Ev")); }
    /// # impl Aggregate for MyAgg { type Event = MyEv; fn apply(&mut self, _: MyEv) {} }
    /// # async fn bad() {
    /// let system = ProcessSystem::new().await;
    /// let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    /// // compile error: spawn() requires CodecSet — call with_codec() first
    /// AggregateProps::<MyAgg>::new(AggregateId::new("x"), store).spawn(&system).await;
    /// # }
    /// ```
    pub async fn spawn(self, system: &ProcessSystem) -> AggregateProxy<A> {
        let codec = self.state.codec;
        let aggregate_id = self.aggregate_id;
        let store = self.store;
        let snapshot_ref = self.snapshot_ref;
        let snapshot_restore = self.snapshot_restore;

        // Capture before moving into the closure so we can pass it to the proxy.
        let aggregate_id_for_proxy = aggregate_id.clone();

        let props = Props::new(move || AggregateProcess {
            state: A::default(),
            aggregate_id: aggregate_id.clone(),
            store: Arc::clone(&store),
            snapshot_ref: snapshot_ref.clone(),
            codec: Arc::clone(&codec),
            sequence: 0,
            snapshot_restore: snapshot_restore.clone(),
        });

        let inner = system.spawn(props).await;
        AggregateProxy::new(inner, aggregate_id_for_proxy)
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
