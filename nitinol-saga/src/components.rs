//! The capabilities the hosting machinery hands a saga when it builds one.
//!
//! # Why they arrive as an argument
//!
//! A saga's own time is resolved by the framework — the resident timer core is
//! the system's, not the saga's — so it cannot be captured by a closure written
//! before the builder that resolves it exists.  Every spawn entry point
//! therefore takes `Fn(`[`SagaComponents`]`<S>) -> S`: the framework resolves what
//! it owns, then calls the producer with it, and the saga's dependencies arrive
//! the way any other function's do.
//!
//! A saga that needs no capability pays nothing for this: `|_| S::default()` is
//! the whole producer.
//!
//! # Why there is no public constructor
//!
//! [`SagaComponents`] can only be obtained from the producer the framework
//! calls, and [`Deadline`] can only be obtained from it.  Asking for one is
//! therefore the declaration that this saga uses that capability, and the
//! declaration is made exactly where the framework can act on it.

pub(crate) mod deadline;

use std::collections::HashMap;
use std::marker::PhantomData;
use std::sync::{Arc, RwLock};

use crate::saga::Saga;
use crate::scheduler::TimerName;
use crate::subscribe::{HasDeadline, SagaSubscribe};

pub use self::deadline::{Deadline, DeadlineMessage, PendingDeadline};

/// The pending deadlines of one saga instance, by the instant each is due.
///
/// Shared between the saga's [`Deadline`] component, which reads it, and the
/// saga process, which writes it as the journal says so — replay's unfired
/// schedules, then each `Scheduled` / `Cancelled` / `Fired` marker that becomes
/// durable.  A view fed by intent instead would disagree with the stream
/// exactly where it matters: after a crash between asking for a deadline and
/// the fact landing.
pub(crate) type PendingDeadlineCell = Arc<RwLock<HashMap<TimerName, jiff::Timestamp>>>;

/// What the framework owns and a saga may ask for.
///
/// Handed to the producer once per incarnation.  It carries no public
/// constructor: a saga reaches a capability through the producer argument or not
/// at all.
///
/// Carries `S` so [`deadline`](Self::deadline) can require `S::Subscribe:
/// SagaSubscribe<Deadline = HasDeadline<M>>` — the same associated type the
/// spawn boundary reads `HAS_DEADLINE` from to decide whether to resolve the
/// resident timer core.  Tying both to the one associated type is what rules
/// out the shape where a schedule fact is persisted and no timer core was
/// ever resolved to arm it.
pub struct SagaComponents<S: Saga> {
    pending_deadlines: PendingDeadlineCell,
    _saga: PhantomData<fn() -> S>,
}

impl<S: Saga> SagaComponents<S> {
    /// Resolve the components of one incarnation.
    ///
    /// Called once per incarnation, so the cell the process writes and the one
    /// the saga's [`Deadline`] reads are the same cell — and a restarted
    /// incarnation starts from an empty one, which replay then fills.
    pub(crate) fn new(pending_deadlines: PendingDeadlineCell) -> Self {
        Self {
            pending_deadlines,
            _saga: PhantomData,
        }
    }

    /// Ask for this saga's own time.
    ///
    /// `M` is the message a deadline carries — the saga's promise to its future
    /// self — and it names the [`Codec`](nitinol_eventsource::codec::Codec) that
    /// writes it into the schedule fact.
    ///
    /// Asking is the declaration: a saga whose producer calls this is one the
    /// host resolves the resident timer core for.  The `where` clause is what
    /// makes that true rather than merely documented: `S::Subscribe:
    /// SagaSubscribe<Deadline = HasDeadline<M>>` holds only when the alphabet
    /// has a `#[subscribe(deadline)]` variant naming `M`, and the spawn
    /// boundary resolves the resident timer core from that exact same
    /// associated type — not from a separately-settable fact that could
    /// disagree with it.  A `Saga` whose alphabet does not declare that
    /// variant has no `deadline::<M>()` to call, so it cannot persist a
    /// schedule fact with no timer core to arm it.
    ///
    /// Asking for `M` when the alphabet declares no `#[subscribe(deadline)]`
    /// variant naming it is a **compile error** — the `where` clause has no
    /// impl to resolve to:
    ///
    /// ```compile_fail
    /// # use async_trait::async_trait;
    /// # use bytes::Bytes;
    /// # use nitinol_eventsource::codec::Codec;
    /// # use nitinol_eventsource::Event;
    /// # use nitinol_persistence::store::{EventStore, InMemoryEventStore};
    /// # use nitinol_persistence::{EventType, Family, TypeName};
    /// # use nitinol_saga::{
    /// #     Deadline, DeadlineMessage, Saga, SagaContext, SagaEffect, SagaId, SagaProps,
    /// # };
    /// # use std::sync::Arc;
    /// # struct DocCodec;
    /// # #[derive(Clone)] struct Nudge;
    /// # #[derive(Clone)] struct Noted;
    /// # impl Codec<Nudge> for DocCodec {
    /// #     type Error = std::convert::Infallible;
    /// #     fn encode(_: &Nudge) -> Result<Bytes, Self::Error> { Ok(Bytes::new()) }
    /// #     fn decode(_: &[u8]) -> Result<Nudge, Self::Error> { Ok(Nudge) }
    /// # }
    /// # impl Codec<Noted> for DocCodec {
    /// #     type Error = std::convert::Infallible;
    /// #     fn encode(_: &Noted) -> Result<Bytes, Self::Error> { Ok(Bytes::new()) }
    /// #     fn decode(_: &[u8]) -> Result<Noted, Self::Error> { Ok(Noted) }
    /// # }
    /// # impl Event for Nudge {
    /// #     const EVENT_TYPE: EventType = EventType::new(Family::new("doc"), TypeName::new("Nudge"));
    /// #     type Codec = DocCodec;
    /// # }
    /// # impl Event for Noted {
    /// #     const EVENT_TYPE: EventType = EventType::new(Family::new("doc"), TypeName::new("Noted"));
    /// #     type Codec = DocCodec;
    /// # }
    /// # use nitinol::saga::SagaSubscribe;
    /// // No `#[subscribe(deadline)]` variant anywhere in this alphabet.
    /// # #[derive(SagaSubscribe)] enum Observed { Nudge(Nudge) }
    /// # struct Reminder;
    /// # struct ReminderCodec;
    /// # impl Codec<Reminder> for ReminderCodec {
    /// #     type Error = std::convert::Infallible;
    /// #     fn encode(_: &Reminder) -> Result<Bytes, Self::Error> { Ok(Bytes::new()) }
    /// #     fn decode(_: &[u8]) -> Result<Reminder, Self::Error> { Ok(Reminder) }
    /// # }
    /// # impl DeadlineMessage for Reminder { type Codec = ReminderCodec; }
    /// struct Undeclared { deadline: Deadline<Reminder> }
    /// # #[async_trait]
    /// # impl Saga for Undeclared {
    /// #     type Subscribe = Observed;
    /// #     type Event = Noted;
    /// #     type Error = std::convert::Infallible;
    /// #     fn correlate(_: &Observed) -> Option<SagaId> { Some(SagaId::new("x")) }
    /// #     async fn handle(&mut self, _: Observed, _: &mut SagaContext)
    /// #         -> Result<SagaEffect<Noted>, Self::Error> { Ok(SagaEffect::None) }
    /// # }
    /// # let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    /// // `Observed`'s `Deadline` is `NoDeadline`, not `HasDeadline<Reminder>`,
    /// // so the `where` clause below has nothing to satisfy this call with.
    /// SagaProps::<Undeclared>::new(SagaId::new("x"), store, |components| Undeclared {
    ///     deadline: components.deadline(),
    /// });
    /// ```
    pub fn deadline<M: DeadlineMessage>(&self) -> Deadline<M>
    where
        S::Subscribe: SagaSubscribe<Deadline = HasDeadline<M>>,
    {
        Deadline::new(Arc::clone(&self.pending_deadlines))
    }
}
