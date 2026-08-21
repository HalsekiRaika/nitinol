//! Resolving an aggregate identifier to a live activation.
//!
//! # What resolve guarantees, and what it does not
//!
//! Within one node, a key has at most one activation: concurrent resolves of
//! the same key cause at most one spawn and every caller receives the same
//! reference (**R-1**).  Across a cluster there is **no such guarantee**.
//! Single activation cluster-wide is a placement goal and a convergence
//! property, not a safety contract (**R-2**) — duplicate activations of one
//! aggregate are a normal occurrence, and correctness rests entirely on the
//! event store's optimistic concurrency contract, which rejects any append
//! derived from a state the stream has already moved past.
//!
//! Two consequences for code written against this layer:
//!
//! * **Never rely on single activation.** Anything whose correctness needs "only
//!   one of these exists" must be arbitrated by the store, not by this registry.
//! * **Store-external side effects are not at-most-once.**
//!   [`Effect::Side`](crate::Effect::Side) and a bare
//!   [`tell`](crate::AggregateProxy::tell) escape the store's arbitration, so a
//!   duplicate activation can perform them twice.  A side effect that must be
//!   cluster-safe belongs behind a persisted record — `Effect::persist` or a
//!   saga outbox — whose own stream OCC decides whether it happened at all.
//!
//! # Resolve does not report which happened
//!
//! Nothing a caller can observe distinguishes "this call activated the
//! aggregate" from "this call joined a live activation" (**R-3**): resolve is an
//! idempotent map from identifier to reference.  The distinction is meaningless
//! the moment an activation may be remote, so no return value or error variant
//! exposes it.

use std::any::{Any, TypeId};
use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use nitinol_persistence::AggregateId;
use nitinol_runtime::ident::Pid;
use nitinol_runtime::process::ProcessProxy;
use nitinol_runtime::{ProcessSystem, Props};
use tokio::sync::watch;

use crate::aggregate::Aggregate;
use crate::process::aggregate_process::AggregateProcess;

/// A handle to one activation of an aggregate.
///
/// This is what the public [`AggregateProxy`](crate::AggregateProxy) used to be.
/// It is deliberately not public: a handle to a single activation is invalidated
/// by that activation's death, which is exactly the property the identity-based
/// reference above it exists to hide.
pub(crate) type Incarnation<A> = ProcessProxy<AggregateProcess<A>>;

/// Builds the process one activation runs.
///
/// Held rather than consumed, because a reference outlives the activation it
/// resolved and must be able to start another one (R-5).
pub(crate) type Activation<A> = Arc<dyn Fn() -> AggregateProcess<A> + Send + Sync>;

/// The identity a reference resolves by: the aggregate type and its stream key.
///
/// Per-activation wiring (which store, which snapshot persistor) is deliberately
/// absent.  Identity decides *which aggregate*, and the first resolve of a key is
/// what activates it — a later resolve naming different wiring joins the running
/// activation rather than starting a second one, because two activations of one
/// stream is the state R-1 exists to prevent.
#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) struct ResolveKey {
    kind: TypeId,
    id: AggregateId,
}

impl ResolveKey {
    pub(crate) fn new<A: Aggregate>(id: AggregateId) -> Self {
        Self {
            kind: TypeId::of::<A>(),
            id,
        }
    }
}

/// A resolved activation, type-erased so one table holds every aggregate type.
#[derive(Clone)]
struct Resident {
    pid: Pid,
    /// An `Arc<Incarnation<A>>` for the `A` whose [`TypeId`] the key carries.
    incarnation: Arc<dyn Any + Send + Sync>,
}

impl Resident {
    fn of<A: Aggregate>(incarnation: &Incarnation<A>) -> Self {
        Self {
            pid: incarnation.pid(),
            incarnation: Arc::new(incarnation.clone()),
        }
    }

    fn downcast<A: Aggregate>(&self) -> Incarnation<A> {
        self.incarnation
            .downcast_ref::<Incarnation<A>>()
            .expect("a key carries the aggregate's TypeId, so its resident is that aggregate's")
            .clone()
    }
}

/// What a key holds while, and after, it is being activated.
enum Slot {
    /// A spawn is in flight; the receiver is published to when it lands.
    Pending(watch::Receiver<Option<Resident>>),
    Ready(Resident),
}

/// The node's aggregate activations, one entry per resolved identity.
///
/// The lock is held only to read or replace a slot — never across the spawn,
/// which replays the aggregate's stream and is unbounded in time.  That is what
/// the [`Slot::Pending`] placeholder buys: latecomers find the key taken and
/// wait on the winner's result instead of racing it (R-1).
pub(crate) struct AggregateRegistry {
    slots: Mutex<HashMap<ResolveKey, Slot>>,
}

impl AggregateRegistry {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            slots: Mutex::new(HashMap::new()),
        })
    }

    fn slots(&self) -> MutexGuard<'_, HashMap<ResolveKey, Slot>> {
        self.slots
            .lock()
            .expect("the activation table is never held across an await, so no holder can panic")
    }

    /// Take the key: join what is there, or claim the right to activate it.
    fn claim(self: &Arc<Self>, key: &ResolveKey) -> Claim {
        let mut slots = self.slots();
        match slots.entry(key.clone()) {
            Entry::Occupied(entry) => match entry.get() {
                Slot::Ready(resident) => Claim::Resident(resident.clone()),
                Slot::Pending(pending) => Claim::Awaiting(pending.clone()),
            },
            Entry::Vacant(entry) => {
                let (publish, pending) = watch::channel(None);
                entry.insert(Slot::Pending(pending));
                Claim::Activate(Activating {
                    registry: Arc::clone(self),
                    key: key.clone(),
                    publish,
                    settled: false,
                })
            }
        }
    }

    fn publish(&self, key: &ResolveKey, resident: Resident) {
        self.slots().insert(key.clone(), Slot::Ready(resident));
    }

    /// Drop a placeholder whose activation never completed.
    fn withdraw(&self, key: &ResolveKey) {
        let mut slots = self.slots();
        if matches!(slots.get(key), Some(Slot::Pending(_))) {
            slots.remove(key);
        }
    }

    /// Drop `pid` from `key`, leaving a later resolve to activate the aggregate
    /// again.
    ///
    /// Matching on the pid is what keeps a slow caller holding a dead reference
    /// from evicting the activation that replaced it.
    fn evict(&self, key: &ResolveKey, pid: Pid) {
        let mut slots = self.slots();
        if matches!(slots.get(key), Some(Slot::Ready(resident)) if resident.pid == pid) {
            slots.remove(key);
        }
    }
}

/// The outcome of taking a key.
enum Claim {
    Resident(Resident),
    Awaiting(watch::Receiver<Option<Resident>>),
    Activate(Activating),
}

/// The right — and the obligation — to activate one key.
///
/// Whoever holds this owns the key's placeholder.  Dropping it without
/// completing removes the placeholder, so a resolve abandoned mid-spawn (a
/// cancelled task, a timed-out caller) can never leave waiters parked on a
/// promise nobody will keep.
struct Activating {
    registry: Arc<AggregateRegistry>,
    key: ResolveKey,
    publish: watch::Sender<Option<Resident>>,
    settled: bool,
}

impl Activating {
    fn settle(mut self, resident: Resident) {
        // Table first, then the waiters: a caller woken by the send must not be
        // able to look the key up and still find it pending.
        self.registry.publish(&self.key, resident.clone());
        // No waiters is the common case, and not a failure.
        let _ = self.publish.send(Some(resident));
        self.settled = true;
    }
}

impl Drop for Activating {
    fn drop(&mut self) {
        if !self.settled {
            self.registry.withdraw(&self.key);
        }
    }
}

/// The system-side wiring a reference needs to resolve itself later.
#[derive(Clone)]
pub(crate) struct ResolveHandle {
    registry: Arc<AggregateRegistry>,
    /// Held because activation happens at dispatch time, long after the entry
    /// point that produced the reference has returned.  A [`ProcessSystem`] is
    /// itself a handle to one instance, so holding it here shares the caller's
    /// system rather than duplicating it.
    system: ProcessSystem,
}

impl ResolveHandle {
    pub(crate) fn new(registry: Arc<AggregateRegistry>, system: ProcessSystem) -> Self {
        Self { registry, system }
    }
}

/// Resolves one aggregate identity to a live activation, over and over.
pub(crate) struct AggregateResolver<A: Aggregate> {
    handle: ResolveHandle,
    key: ResolveKey,
    activation: Activation<A>,
}

impl<A: Aggregate> Clone for AggregateResolver<A> {
    fn clone(&self) -> Self {
        Self {
            handle: self.handle.clone(),
            key: self.key.clone(),
            activation: Arc::clone(&self.activation),
        }
    }
}

impl<A: Aggregate> AggregateResolver<A> {
    pub(crate) fn new(handle: ResolveHandle, id: AggregateId, activation: Activation<A>) -> Self {
        Self {
            handle,
            key: ResolveKey::new::<A>(id),
            activation,
        }
    }

    /// The live activation of this identity, starting one if there is none.
    pub(crate) async fn resolve(&self) -> Incarnation<A> {
        loop {
            match self.handle.registry.claim(&self.key) {
                Claim::Resident(resident) => return resident.downcast::<A>(),
                Claim::Awaiting(pending) => {
                    if let Some(resident) = await_resident(pending).await {
                        return resident.downcast::<A>();
                    }
                    // The winner was dropped mid-spawn and took its placeholder
                    // with it; race for the key again rather than wait forever.
                }
                Claim::Activate(claim) => {
                    let incarnation = self.activate().await;
                    claim.settle(Resident::of(&incarnation));
                    return incarnation;
                }
            }
        }
    }

    async fn activate(&self) -> Incarnation<A> {
        let activation = Arc::clone(&self.activation);
        self.handle
            .system
            .spawn(Props::new(move || activation()))
            .await
    }

    /// Report `pid` as no longer serving this identity.
    pub(crate) fn evict(&self, pid: Pid) {
        self.handle.registry.evict(&self.key, pid);
    }
}

/// The activation a placeholder resolves to, or `None` if its owner vanished.
async fn await_resident(mut pending: watch::Receiver<Option<Resident>>) -> Option<Resident> {
    loop {
        // Read and release before awaiting: the borrow guard is not held across
        // the suspension point.
        let published = pending.borrow().clone();
        if published.is_some() {
            return published;
        }
        pending.changed().await.ok()?;
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    /// The key under test.  Nothing here ever publishes a resident, so which
    /// aggregate type the `TypeId` names does not matter.
    fn key() -> ResolveKey {
        ResolveKey {
            kind: TypeId::of::<()>(),
            id: AggregateId::new("abandoned"),
        }
    }

    /// The cleanup half of R-1: an activation abandoned before it completes must
    /// release its waiters and leave the key free.
    ///
    /// A resolve can be dropped mid-spawn — a cancelled task, a caller that
    /// stopped waiting — and the placeholder it installed would otherwise stay in
    /// the table for good, parking every later resolve of that aggregate on a
    /// promise nobody is left to keep.  A permanently unreachable aggregate is a
    /// worse failure than the abandoned spawn that caused it.
    #[tokio::test]
    async fn an_abandoned_activation_releases_its_waiters_and_frees_the_key() {
        // Given: one caller holds the key, and another is waiting on it
        let registry = AggregateRegistry::new();
        let Claim::Activate(claim) = registry.claim(&key()) else {
            panic!("the first claim on an empty table must be the right to activate");
        };
        let Claim::Awaiting(waiting) = registry.claim(&key()) else {
            panic!("a claim on a key already being activated must wait for it");
        };

        // When: the holder goes away without activating anything
        drop(claim);

        // Then
        let published = tokio::time::timeout(Duration::from_secs(5), await_resident(waiting))
            .await
            .expect("a waiter must not stay parked on an abandoned activation");
        assert!(
            published.is_none(),
            "an abandoned activation has no result to hand out"
        );
        assert!(
            matches!(registry.claim(&key()), Claim::Activate(_)),
            "the abandoned key must be free for the next caller to activate"
        );
    }

    /// A placeholder is withdrawn only by the caller that installed it.
    ///
    /// Without that, an abandoned resolve arriving late would drop the
    /// placeholder of the activation that had since replaced it, and two callers
    /// would go on to activate the same aggregate twice.
    #[tokio::test]
    async fn withdrawing_leaves_a_later_activation_of_the_same_key_alone() {
        // Given: the key was claimed, abandoned, and claimed again
        let registry = AggregateRegistry::new();
        let Claim::Activate(first) = registry.claim(&key()) else {
            panic!("the first claim on an empty table must be the right to activate");
        };
        drop(first);
        let Claim::Activate(second) = registry.claim(&key()) else {
            panic!("the abandoned key must be claimable again");
        };

        // When / Then: the key is held by the second claim, not free
        assert!(
            matches!(registry.claim(&key()), Claim::Awaiting(_)),
            "a third caller must wait for the second claim rather than activate in parallel"
        );

        drop(second);
    }
}
