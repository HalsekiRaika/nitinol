use std::sync::{Arc, Mutex};

use nitinol_contract::{Aggregate, Decider, Query};
use nitinol_persistence::error::AppendError;
use nitinol_persistence::AggregateId;
use nitinol_runtime::error::AskError as RuntimeAskError;

use crate::error::{
    AskError, AskHandlerError, ExecError, ExecHandlerError, PersistError, TellError,
};
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
/// running" handle to choose between.  What varies is how the reference was
/// obtained, not what it means.
///
/// | Origin | Behaviour |
/// |---|---|
/// | [`EventSourceSystem`](crate::system::EventSourceSystem) — `spawn_aggregate`, `aggregate_props(..).spawn(..)`, `aggregate_proxy` | Resolves through the node's registry: at most one activation per key on this node, re-resolved after an activation dies |
/// | [`AggregateProps::spawn`](crate::AggregateProps::spawn) built directly | Pinned to the activation that call started — an explicit lifecycle, not a resolve |
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
    binding: Arc<Binding<A>>,
}

/// How a reference reaches an activation.
enum Binding<A: Aggregate> {
    /// The one activation an explicit `AggregateProps::spawn` started.
    ///
    /// That call starts a lifecycle rather than resolving an identity, so its
    /// reference stays with what it started and does not replace it.
    Pinned(Incarnation<A>),
    /// An identity, resolved on demand.
    ///
    /// The cache is an optimisation, not the reference's meaning: emptying it
    /// costs a resolve, never a lost dispatch.
    Resolved {
        resolver: AggregateResolver<A>,
        cached: Mutex<Option<Incarnation<A>>>,
    },
}

impl<A: Aggregate> Clone for AggregateProxy<A> {
    fn clone(&self) -> Self {
        Self {
            aggregate_id: self.aggregate_id.clone(),
            binding: Arc::clone(&self.binding),
        }
    }
}

impl<A: Aggregate> AggregateProxy<A> {
    /// A reference bound to the activation `incarnation`, for the explicit
    /// lifecycle path.
    pub(crate) fn pinned(aggregate_id: AggregateId, incarnation: Incarnation<A>) -> Self {
        Self {
            aggregate_id,
            binding: Arc::new(Binding::Pinned(incarnation)),
        }
    }

    /// A reference that resolves `aggregate_id` through the registry.
    ///
    /// Nothing is activated here; the first dispatch — or an explicit
    /// [`activate`][Self::activate] — does that.
    pub(crate) fn resolved(aggregate_id: AggregateId, resolver: AggregateResolver<A>) -> Self {
        Self {
            aggregate_id,
            binding: Arc::new(Binding::Resolved {
                resolver,
                cached: Mutex::new(None),
            }),
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
        let outcome = incarnation.ask(AskCmd(cmd)).await.map_err(map_ask_error);
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
        let outcome = incarnation.ask(ExecMsg(msg)).await.map_err(map_exec_error);
        if matches!(outcome, Err(ExecError::Send(_))) {
            self.invalidate(&incarnation);
        }
        outcome
    }

    /// The activation this dispatch goes to.
    async fn incarnation(&self) -> Incarnation<A> {
        match &*self.binding {
            Binding::Pinned(incarnation) => incarnation.clone(),
            Binding::Resolved { resolver, cached } => {
                let hit = cached.lock().expect(CACHE_LOCK).clone();
                if let Some(incarnation) = hit {
                    return incarnation;
                }
                // Two dispatches racing here both resolve, and the registry's
                // single flight hands them the same activation.
                let incarnation = resolver.resolve().await;
                *cached.lock().expect(CACHE_LOCK) = Some(incarnation.clone());
                incarnation
            }
        }
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
        let Binding::Resolved { resolver, cached } = &*self.binding else {
            return;
        };

        {
            let mut cached = cached.lock().expect(CACHE_LOCK);
            if cached.as_ref().map(|live| live.pid()) == Some(dead.pid()) {
                *cached = None;
            }
        }

        resolver.evict(dead.pid());
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

// Error mappers

fn map_ask_error<R>(e: RuntimeAskError<AskHandlerError<R>>) -> AskError<R>
where
    R: std::error::Error + Send + Sync + 'static,
{
    match e {
        RuntimeAskError::Handler(h) => match h {
            AskHandlerError::Rejection(r) => AskError::Rejection(r),
            AskHandlerError::AlreadyCreated => AskError::AlreadyCreated,
            AskHandlerError::Persist(e) => AskError::Persist(e),
        },
        RuntimeAskError::DeadLetter { .. }
        | RuntimeAskError::ReplyDropped
        | RuntimeAskError::Timeout { .. } => AskError::Send(nitinol_runtime::error::SendError),
    }
}

fn map_exec_error<E>(e: RuntimeAskError<ExecHandlerError<E>>) -> ExecError<E>
where
    E: std::error::Error + Send + Sync + 'static,
{
    match e {
        RuntimeAskError::Handler(h) => match h {
            ExecHandlerError::Domain(e) => ExecError::Domain(e),
        },
        RuntimeAskError::DeadLetter { .. }
        | RuntimeAskError::ReplyDropped
        | RuntimeAskError::Timeout { .. } => ExecError::Send(nitinol_runtime::error::SendError),
    }
}
