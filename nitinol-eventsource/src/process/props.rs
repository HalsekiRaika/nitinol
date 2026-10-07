use std::sync::Arc;

use nitinol_contract::{Aggregate, Snapshotable};
use nitinol_persistence::store::EventStore;
use nitinol_persistence::AggregateId;
use nitinol_runtime::ProcessSystem;

use crate::codec;
use crate::process::aggregate_process::{AggregateProcess, SnapshotRestoreFn};
use crate::process::proxy::AggregateProxy;
use crate::process::resolve::{self, Activation, AggregateResolver};
use crate::process::snapshot_persistor::SnapshotPersistorProxy;
use crate::system::{Hosted, SystemHandle, Unhosted};

/// How one activation of an aggregate is configured.
///
/// `H` is where the props start. Built here directly they are [`Unhosted`], and
/// [`spawn`][Self::spawn] takes the [`ProcessSystem`] to start a lifecycle on.
/// Obtained from an [`EventSourceSystem`](crate::system::EventSourceSystem)
/// they are [`Hosted`], and `spawn` takes nothing: the system they came from is
/// where they start, and its registry resolves the aggregate rather than
/// activating it per call.
pub struct AggregateProps<A: Aggregate, H = Unhosted> {
    aggregate_id: AggregateId,
    store: Arc<dyn EventStore>,
    snapshot_ref: Option<SnapshotPersistorProxy>,
    snapshot_restore: Option<SnapshotRestoreFn<A>>,
    host: H,
}

impl<A: Aggregate> AggregateProps<A, Unhosted> {
    pub fn new(aggregate_id: AggregateId, store: Arc<dyn EventStore>) -> Self {
        Self {
            aggregate_id,
            store,
            snapshot_ref: None,
            snapshot_restore: None,
            host: Unhosted,
        }
    }

    /// Start a lifecycle for this aggregate on `system`.
    ///
    /// Every call is another activation, and the reference stays with the one
    /// it started. Two activations of one stream both believe they are its only
    /// writer, so all but one of them will lose an append and stop — which is
    /// why a deployment with a system takes its props from there instead.
    ///
    /// There is no codec step: `A::Event` names the codec that writes it, so
    /// the activation resolves the format from the event type and nothing has
    /// to be handed one here.
    pub async fn spawn(self, system: &ProcessSystem) -> AggregateProxy<A> {
        let (aggregate_id, activation) = self.into_parts();
        let proxy = system.spawn(resolve::activation_props(activation)).await;
        AggregateProxy::pinned(aggregate_id, proxy)
    }

    /// Carry `host` as where these props start.
    pub(crate) fn hosted(self, host: SystemHandle) -> AggregateProps<A, Hosted> {
        AggregateProps {
            aggregate_id: self.aggregate_id,
            store: self.store,
            snapshot_ref: self.snapshot_ref,
            snapshot_restore: self.snapshot_restore,
            host: Hosted::new(host),
        }
    }
}

impl<A: Aggregate> AggregateProps<A, Hosted> {
    /// Resolve this aggregate on the system these props came from, activating
    /// it if that system has no activation for it, and return a reference.
    ///
    /// Nothing is named here: the process system, the registry and the journal
    /// are all the ones the props were taken from. The configuration they carry
    /// is used only if this call is the one that activates the aggregate,
    /// because a second activation of one stream is what the resolve layer
    /// exists to prevent.
    pub async fn spawn(self) -> AggregateProxy<A> {
        // "Resolve *and activate*" — the caller is promised a running
        // aggregate, whether or not this call is what started it.
        let reference = self.into_reference();
        reference.activate().await;
        reference
    }

    /// A reference that resolves this aggregate at dispatch time, activating
    /// nothing yet.
    ///
    /// The synchronous half of the resolve entry points: a caller that cannot
    /// await — a `fold` over a decision, a crash-restart factory — still gets a
    /// reference addressing the same aggregate every other entry point does.
    pub(crate) fn into_reference(self) -> AggregateProxy<A> {
        // The handle carries the system a later re-resolve spawns on, so the
        // activation this call may start and the one that replaces it are
        // identical.
        let handle = self.host.system().resolve_handle();
        let (aggregate_id, activation) = self.into_parts();
        let resolver = AggregateResolver::new(handle, aggregate_id.clone(), activation);
        AggregateProxy::resolved(aggregate_id, resolver)
    }
}

impl<A: Aggregate, H> AggregateProps<A, H> {
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
            host: _,
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

impl<A: Aggregate + Snapshotable, H> AggregateProps<A, H> {
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
