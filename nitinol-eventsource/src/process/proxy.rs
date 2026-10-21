use std::sync::{Arc, Mutex};

use nitinol_contract::{Aggregate, Decider, Query};
use nitinol_persistence::error::AppendError;
use nitinol_persistence::AggregateId;

use crate::error::{AskError, ExecError, PersistError, TellError};
use crate::process::aggregate_process::{AskCmd, ExecMsg, TellCmd};
use crate::process::resolve::{AggregateResolver, Incarnation};

/// A reference to an aggregate, addressed by identity rather than by activation.
///
/// The reference names *which aggregate* — its stream key — and finds an
/// activation to carry each dispatch.  It does not name a process, and it does
/// not become invalid when one dies: an activation that stops (including one
/// stopped by losing its stream to another writer — see below) is dropped from
/// the reference, and the next dispatch resolves the aggregate again.  A
/// reference is therefore safe to keep for as long as the aggregate is
/// meaningful, in a saga's captured state or a long-lived router.
///
/// # Resolution is silent about what it did
///
/// No return value, error variant or observable timing tells a caller whether a
/// dispatch activated the aggregate or joined a live activation.  The
/// distinction stops being expressible once activations may be remote, so code
/// must not be written against it.
///
/// # One reference type
///
/// This is the only reference type in the framework: there is no second "already
/// running" handle to choose between, and no second way of reaching one.  Every
/// entry point — [`EventSourceSystem::spawn_aggregate`](crate::system::EventSourceSystem::spawn_aggregate),
/// [`aggregate_props(..).spawn()`](crate::system::EventSourceSystem::aggregate_props),
/// [`aggregate_proxy`](crate::system::EventSourceSystem::aggregate_proxy) and
/// [`AggregateProps::spawn`](crate::AggregateProps::spawn) on props built
/// directly — resolves through the node's registry: at most one activation per
/// key on this node, re-resolved after an activation dies.
///
/// # Duplicate activations are normal
///
/// The at-most-one guarantee is per node.  Cluster-wide, one aggregate may have
/// more than one activation at a time; only the event store's OCC decides which
/// writes count.  See [`EventSourceSystem`](crate::system::EventSourceSystem)
/// for what that means for side effects.
pub struct AggregateProxy<A: Aggregate> {
    /// The aggregate's stream key — kept here so
    /// [`AggregateTellTarget::aggregate_id`](crate::AggregateTellTarget::aggregate_id)
    /// can answer without resolving anything.
    aggregate_id: AggregateId,
    /// The one [`Resolution`] this reference and its clones answer from.
    ///
    /// Another name for what it reaches rather than a copy of it, so a
    /// reference is shared by cloning it.
    resolution: Resolution<A>,
}

/// One identity's resolver and the activation it last resolved to.
///
/// The cache is an optimisation, not the reference's meaning: emptying it costs
/// a resolve, never a lost dispatch.
///
/// Cheap-clone, and a clone names the same resolver and the same cache: the
/// death one clone of a reference observes is a death every clone is spared
/// dispatching into, which is the whole of what the cache is for.
struct Resolution<A: Aggregate> {
    inner: Arc<ResolutionInner<A>>,
}

struct ResolutionInner<A: Aggregate> {
    resolver: AggregateResolver<A>,
    cached: Mutex<Option<Incarnation<A>>>,
}

impl<A: Aggregate> Clone for Resolution<A> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<A: Aggregate> Resolution<A> {
    fn new(resolver: AggregateResolver<A>) -> Self {
        Self {
            inner: Arc::new(ResolutionInner {
                resolver,
                cached: Mutex::new(None),
            }),
        }
    }

    /// The live activation of this identity, resolving one when the cache is
    /// empty.
    async fn incarnation(&self) -> Incarnation<A> {
        let hit = self.inner.cached.lock().expect(CACHE_LOCK).clone();
        if let Some(incarnation) = hit {
            return incarnation;
        }
        // Two dispatches racing here both resolve, and the registry's single
        // flight hands them the same activation.
        let incarnation = self.inner.resolver.resolve().await;
        *self.inner.cached.lock().expect(CACHE_LOCK) = Some(incarnation.clone());
        incarnation
    }

    /// Report `dead` as gone, emptying the cache only while it is still the
    /// activation the cache names.
    fn invalidate(&self, dead: &Incarnation<A>) {
        {
            let mut cached = self.inner.cached.lock().expect(CACHE_LOCK);
            if cached.as_ref().map(|live| live.pid()) == Some(dead.pid()) {
                *cached = None;
            }
        }

        self.inner.resolver.evict(dead.pid());
    }
}

impl<A: Aggregate> Clone for AggregateProxy<A> {
    fn clone(&self) -> Self {
        Self {
            aggregate_id: self.aggregate_id.clone(),
            resolution: self.resolution.clone(),
        }
    }
}

impl<A: Aggregate> AggregateProxy<A> {
    /// A reference that resolves `aggregate_id` through the registry.
    ///
    /// Nothing is activated here; the first dispatch — or an explicit
    /// [`activate`][Self::activate] — does that.
    pub(crate) fn resolved(aggregate_id: AggregateId, resolver: AggregateResolver<A>) -> Self {
        Self {
            aggregate_id,
            resolution: Resolution::new(resolver),
        }
    }

    /// Resolve now, so an entry point that promises a running aggregate returns
    /// with one.
    pub(crate) async fn activate(&self) {
        self.incarnation().await;
    }

    /// The aggregate's stream key.
    pub fn aggregate_id(&self) -> &AggregateId {
        &self.aggregate_id
    }

    /// Send a command and wait for the answer its decision states.
    ///
    /// Returns the decider's `Output` — what the command asked for — rather than
    /// the events it produced.  The events are the aggregate's own record of
    /// what happened and are read from the stream by whoever needs them; a
    /// caller that had to derive its answer from them would be deriving it from
    /// facts the decider already read to reach the answer.
    ///
    /// [`AskError::retryability`] separates a failure that says something about
    /// the command from one that says something about where it was sent.
    pub async fn ask<C>(
        &self,
        cmd: C,
    ) -> Result<<A as Decider<C>>::Output, AskError<<A as Decider<C>>::Rejection>>
    where
        A: Decider<C>,
        C: Send + Sync + 'static,
        <A as Decider<C>>::Output: Send + 'static,
        <A as Decider<C>>::Rejection: std::error::Error + Send + Sync + 'static,
    {
        let incarnation = self.incarnation().await;
        let outcome = incarnation.ask(AskCmd(cmd)).await.map_err(AskError::from);
        if signals_stopped_activation(&outcome) {
            self.invalidate(&incarnation);
        }
        outcome
    }

    /// Send a command without waiting for a response.
    ///
    /// The command is queued and processed in FIFO order; ordering relative to
    /// later `exec` calls is guaranteed by the single-threaded process loop.
    ///
    /// The decision's output is discarded — nobody is waiting for it — but a
    /// refusal is reported through the crate's tracing records rather than
    /// dropped, so a command that was refused is not mistaken for one that was
    /// carried out (L-5).  `Ok(())` therefore means the command was accepted for
    /// delivery, not that the aggregate accepted it.
    pub async fn tell<C>(&self, cmd: C) -> Result<(), TellError>
    where
        A: Decider<C>,
        C: Send + Sync + 'static,
        <A as Decider<C>>::Rejection: std::error::Error + Send + Sync + 'static,
    {
        let incarnation = self.incarnation().await;
        let outcome = incarnation
            .tell(TellCmd(cmd))
            .await
            .map_err(TellError::Send);
        if outcome.is_err() {
            self.invalidate(&incarnation);
        }
        outcome
    }

    /// Send a read-only query and wait for the response.
    ///
    /// `exec` does not mutate aggregate state.
    pub async fn exec<M>(
        &self,
        msg: M,
    ) -> Result<<A as Query<M>>::Response, ExecError<<A as Query<M>>::Error>>
    where
        A: Query<M>,
        M: Send + Sync + 'static,
        <A as Query<M>>::Response: Send + 'static,
        <A as Query<M>>::Error: std::error::Error + Send + Sync + 'static,
    {
        let incarnation = self.incarnation().await;
        let outcome = incarnation.ask(ExecMsg(msg)).await.map_err(ExecError::from);
        if matches!(outcome, Err(ExecError::Send(_))) {
            self.invalidate(&incarnation);
        }
        outcome
    }

    /// The activation this dispatch goes to.
    async fn incarnation(&self) -> Incarnation<A> {
        self.resolution.incarnation().await
    }

    /// Treat `dead` as gone, so the next dispatch resolves a live activation.
    ///
    /// Two things count as the invalidation signal: a send that does not reach
    /// its destination, and a non-genesis `SequenceConflict` — an overtaken
    /// writer stops the instant it loses the race, before this call returns, so
    /// waiting for a *later* send to fail against it would cost the very next
    /// dispatch a doomed round-trip. Nothing is retried here — the caller
    /// learns the dispatch failed, and [`AskError::retryability`] tells it that
    /// retrying is worthwhile.
    fn invalidate(&self, dead: &Incarnation<A>) {
        self.resolution.invalidate(dead);
    }
}

/// Why the incarnation cache lock cannot be poisoned: it guards a `clone` and an
/// assignment, and is never held across an await.
const CACHE_LOCK: &str = "the incarnation cache is never held across an await, so no holder panics";

/// Whether `outcome` means the activation `ask` dispatched to can no longer be
/// reached.
///
/// A `Send` failure is the reactive case: the destination was already gone
/// when the runtime tried to deliver to it. A non-genesis `SequenceConflict`
/// is the proactive case: `AggregateProcess` answers it only after calling
/// `stop_self`, so an `Append(SequenceConflict)` reaching here always
/// names an activation that is on its way out, whether or not its mailbox has
/// closed yet.
///
/// Nothing else qualifies. A backend failure or a codec error says nothing
/// about whether the activation is still alive, and
/// [`AskError::AlreadyCreated`] says the opposite of C-1: the activation
/// recognised the collision and lives on (C-2). Evicting on any of them would
/// cost a resolve nobody asked for and hand the next dispatch a different
/// activation than the one that answered this one.
fn signals_stopped_activation<T, R>(outcome: &Result<T, AskError<R>>) -> bool
where
    R: std::error::Error + Send + Sync + 'static,
{
    matches!(
        outcome,
        Err(AskError::Send(_))
            | Err(AskError::Persist(PersistError::Append(
                AppendError::SequenceConflict(_)
            )))
    )
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use nitinol_contract::{Codec, Never};
    use nitinol_persistence::provider::PersistenceProvider;
    use nitinol_persistence::store::InMemoryStore;
    use nitinol_persistence::{EventType, Family, TypeName};
    use nitinol_runtime::ProcessSystem;

    use super::*;
    use crate::process::props::AggregateProps;
    use crate::system::EventSourceSystem;

    struct BumpedCodec;

    impl Codec<Bumped> for BumpedCodec {
        type Error = std::convert::Infallible;

        fn encode(_event: &Bumped) -> Result<Bytes, Self::Error> {
            Ok(Bytes::new())
        }

        fn decode(_payload: &[u8]) -> Result<Bumped, Self::Error> {
            Ok(Bumped)
        }
    }

    #[derive(Clone, Debug, PartialEq)]
    struct Bumped;

    impl nitinol_contract::Event for Bumped {
        const EVENT_TYPE: EventType =
            EventType::new(Family::new("proxy.pid"), TypeName::new("Bumped"));
        type Codec = BumpedCodec;
    }

    /// Carries no snapshot policy: this test only exercises which activation a
    /// reference resolves to, not recovery.
    #[derive(Default)]
    struct Counted;

    impl Aggregate for Counted {
        type Event = Bumped;
        type Recover = Never;

        fn apply(&mut self, _event: Bumped) {}
    }

    /// Both entry points reach one activation, restated at the layer that owns
    /// the pid.
    ///
    /// A reference does not expose a pid to its caller (see "Resolution is
    /// silent about what it did" above), so
    /// `nitinol-eventsource/tests/props_take_their_stores_from_the_system.rs`
    /// observes the same clause from outside through the store's optimistic
    /// concurrency rule. This module is the one place allowed to look past
    /// that and compare pids directly, because it is where the resolution
    /// cache they come from is defined.
    #[tokio::test]
    async fn both_entry_points_resolve_to_the_same_activation() {
        let system: EventSourceSystem<InMemoryStore> =
            EventSourceSystem::builder(ProcessSystem::new().await)
                .with_persistence(PersistenceProvider::new(InMemoryStore::default()))
                .build();
        let id = AggregateId::new("proxy-pid-one-activation");

        let via_props = AggregateProps::<Counted>::new(id.clone())
            .spawn(&system)
            .await;
        let via_system = system.aggregate_props::<Counted>(id).spawn().await;

        assert_eq!(
            via_props.incarnation().await.pid(),
            via_system.incarnation().await.pid(),
            "the props-first and system-first entry points must resolve the \
             same identity to one activation, not two"
        );
    }
}
