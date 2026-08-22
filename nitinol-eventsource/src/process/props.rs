use std::sync::Arc;

use nitinol_persistence::store::EventStore;
use nitinol_persistence::AggregateId;
use nitinol_runtime::{ProcessSystem, Props};

use crate::aggregate::{Aggregate, Snapshotable};
use crate::codec::ErasedCodec;
use crate::process::aggregate_process::{AggregateProcess, SnapshotRestoreFn};
use crate::process::proxy::AggregateProxy;
use crate::process::resolve::{Activation, AggregateResolver, ResolveHandle};
use crate::process::snapshot_persistor::SnapshotPersistorProxy;

pub struct CodecUnset;

pub struct CodecSet<E> {
    pub(crate) codec: Arc<dyn ErasedCodec<E>>,
}

/// How one activation of an aggregate is configured.
///
/// Which entry point produced the props decides what [`spawn`][Self::spawn]
/// means.  Built here directly, `spawn` starts a lifecycle and hands back a
/// reference pinned to it.  Obtained from an
/// [`EventSourceSystem`](crate::system::EventSourceSystem), `spawn` resolves the
/// aggregate through that system's registry: the configuration below is used
/// only if this call is the one that activates it, because a second activation
/// of one stream is what the resolve layer exists to prevent.
pub struct AggregateProps<A: Aggregate, S = CodecUnset> {
    aggregate_id: AggregateId,
    store: Arc<dyn EventStore>,
    snapshot_ref: Option<SnapshotPersistorProxy>,
    snapshot_restore: Option<SnapshotRestoreFn<A>>,
    /// Set when these props came from an `EventSourceSystem`, whose registry
    /// resolves the aggregate rather than activating it per call.
    resolve: Option<ResolveHandle>,
    state: S,
}

impl<A: Aggregate> AggregateProps<A, CodecUnset> {
    pub fn new(aggregate_id: AggregateId, store: Arc<dyn EventStore>) -> Self {
        Self {
            aggregate_id,
            store,
            snapshot_ref: None,
            snapshot_restore: None,
            resolve: None,
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
            resolve: self.resolve,
            state: CodecSet { codec },
        }
    }
}

impl<A: Aggregate, S> AggregateProps<A, S> {
    /// Route this spawn through `handle`'s registry instead of activating per call.
    pub(crate) fn with_resolve(mut self, handle: ResolveHandle) -> Self {
        self.resolve = Some(handle);
        self
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
    ///
    /// # Two meanings
    ///
    /// Props built through an [`EventSourceSystem`](crate::system::EventSourceSystem)
    /// **resolve**: the aggregate is activated only if it is not already, and the
    /// reference returned survives that activation's death, resolving the
    /// aggregate again on its next dispatch.  Props built directly **start a
    /// lifecycle**: every call is another activation, and the reference stays
    /// with the one it started.  Two activations of one stream both believe they
    /// are its only writer, so all but one of them will lose an append and stop.
    pub async fn spawn(self, system: &ProcessSystem) -> AggregateProxy<A> {
        match self.resolve.clone() {
            Some(handle) => {
                // The handle carries the system a later re-resolve spawns on, so
                // `system` is not consulted here: the activation this call may
                // start and the one that replaces it must be identical.
                let reference = self.into_reference(handle);
                // "Resolve *and activate*" — the caller is promised a running
                // aggregate, whether or not this call is what started it.
                reference.activate().await;
                reference
            }
            None => {
                let (aggregate_id, activation) = self.into_parts();
                let incarnation = system.spawn(Props::new(move || activation())).await;
                AggregateProxy::pinned(aggregate_id, incarnation)
            }
        }
    }

    /// A reference that resolves this aggregate at dispatch time, activating
    /// nothing yet.
    ///
    /// The synchronous half of the resolve entry points: a caller that cannot
    /// await — a `fold` over a decision, a crash-restart factory — still gets a
    /// reference addressing the same aggregate every other entry point does.
    pub(crate) fn into_reference(self, handle: ResolveHandle) -> AggregateProxy<A> {
        let (aggregate_id, activation) = self.into_parts();
        let resolver = AggregateResolver::new(handle, aggregate_id.clone(), activation);
        AggregateProxy::resolved(aggregate_id, resolver)
    }

    /// The identity and the recipe one activation runs.
    ///
    /// The sole place a configured props becomes a runnable activation, so a
    /// reference that re-resolves after a death starts the same process the
    /// original entry point did.
    fn into_parts(self) -> (AggregateId, Activation<A>) {
        let AggregateProps {
            aggregate_id,
            store,
            snapshot_ref,
            snapshot_restore,
            resolve: _,
            state: CodecSet { codec },
        } = self;

        let identity = aggregate_id.clone();
        let activation: Activation<A> = Arc::new(move || AggregateProcess {
            state: A::default(),
            aggregate_id: aggregate_id.clone(),
            store: Arc::clone(&store),
            snapshot_ref: snapshot_ref.clone(),
            codec: Arc::clone(&codec),
            sequence: 0,
            snapshot_restore: snapshot_restore.clone(),
        });

        (identity, activation)
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
