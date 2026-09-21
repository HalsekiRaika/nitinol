use std::sync::Arc;

use nitinol_contract::{Aggregate, Snapshotable};
use nitinol_persistence::store::EventStore;
use nitinol_persistence::AggregateId;
use nitinol_runtime::ProcessSystem;

use crate::codec;
use crate::process::aggregate_process::{AggregateProcess, SnapshotRestoreFn};
use crate::process::proxy::AggregateProxy;
use crate::process::resolve::{self, Activation, AggregateResolver, ResolveHandle};
use crate::process::snapshot_persistor::SnapshotPersistorProxy;

/// How one activation of an aggregate is configured.
///
/// Which entry point produced the props decides what [`spawn`][Self::spawn]
/// means.  Built here directly, `spawn` starts a lifecycle and hands back a
/// reference pinned to it.  Obtained from an
/// [`EventSourceSystem`](crate::system::EventSourceSystem), `spawn` resolves the
/// aggregate through that system's registry: the configuration below is used
/// only if this call is the one that activates it, because a second activation
/// of one stream is what the resolve layer exists to prevent.
pub struct AggregateProps<A: Aggregate> {
    aggregate_id: AggregateId,
    store: Arc<dyn EventStore>,
    snapshot_ref: Option<SnapshotPersistorProxy>,
    snapshot_restore: Option<SnapshotRestoreFn<A>>,
    /// Set when these props came from an `EventSourceSystem`, whose registry
    /// resolves the aggregate rather than activating it per call.
    resolve: Option<ResolveHandle>,
}

impl<A: Aggregate> AggregateProps<A> {
    pub fn new(aggregate_id: AggregateId, store: Arc<dyn EventStore>) -> Self {
        Self {
            aggregate_id,
            store,
            snapshot_ref: None,
            snapshot_restore: None,
            resolve: None,
        }
    }

    /// Route this spawn through `handle`'s registry instead of activating per call.
    pub(crate) fn with_resolve(mut self, handle: ResolveHandle) -> Self {
        self.resolve = Some(handle);
        self
    }

    /// Spawn this aggregate process.
    ///
    /// There is no codec step: `A::Event` names the codec that writes it, so
    /// the activation resolves the format from the event type and nothing has
    /// to be handed one here.
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
                let proxy = system.spawn(resolve::activation_props(activation)).await;
                AggregateProxy::pinned(aggregate_id, proxy)
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
        } = self;

        let identity = aggregate_id.clone();
        let activation: Activation<A> = Arc::new(move || AggregateProcess {
            state: A::default(),
            aggregate_id: aggregate_id.clone(),
            store: Arc::clone(&store),
            snapshot_ref: snapshot_ref.clone(),
            sequence: 0,
            snapshot_restore: snapshot_restore.clone(),
        });

        (identity, activation)
    }
}

impl<A: Aggregate + Snapshotable> AggregateProps<A> {
    /// Restore this aggregate from the snapshots `snapshot_ref` holds.
    ///
    /// No codec argument: the format is `<A as Snapshotable>::Codec`, which the
    /// aggregate named alongside the snapshot value it writes.
    pub fn with_snapshot_persistor(mut self, snapshot_ref: SnapshotPersistorProxy) -> Self {
        self.snapshot_ref = Some(snapshot_ref);
        self.snapshot_restore = Some(Arc::new(|payload: &[u8]| {
            codec::decode::<<A as Snapshotable>::Codec, A::Snapshot>(payload).map(A::restore)
        }));
        self
    }
}
