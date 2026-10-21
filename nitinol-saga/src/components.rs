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

use std::marker::PhantomData;

use nitinol_eventsource::system::SystemHandle;

use crate::saga::Saga;
use crate::subscribe::{HasDeadline, SagaSubscribe};

pub(crate) use self::deadline::PendingDeadlines;
pub use self::deadline::{Deadline, DeadlineMessage, PendingDeadline};

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
    pending_deadlines: PendingDeadlines,
    host: SystemHandle,
    _saga: PhantomData<fn() -> S>,
}

impl<S: Saga> SagaComponents<S> {
    /// Resolve the components of one incarnation.
    ///
    /// Called once per incarnation, so the cell the process writes and the one
    /// the saga's [`Deadline`] reads are the same cell — and a restarted
    /// incarnation starts from an empty one, which replay then fills.
    pub(crate) fn new(pending_deadlines: PendingDeadlines, host: SystemHandle) -> Self {
        Self {
            pending_deadlines,
            host,
            _saga: PhantomData,
        }
    }

    /// The system hosting this saga.
    ///
    /// Not an `Option`: every saga is started on a system — whether its wiring
    /// was written props-first as
    /// [`SagaManagerProps::new`](crate::SagaManagerProps::new)`(..).spawn(&system)`
    /// or system-first as
    /// [`SagaSystemExt::host`](crate::SagaSystemExt::host) — so there is always
    /// one to answer with.
    ///
    /// The handle reaches the host's process system, its aggregate registry and
    /// the journal its supplier distributes — so a saga that has to tell an
    /// aggregate resolves the reference from here rather than from a clone of
    /// the system captured outside the spawn.
    pub fn system(&self) -> &SystemHandle {
        &self.host
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
    /// # use nitinol_persistence::{EventType, Family, TypeName};
    /// # use nitinol_saga::{
    /// #     Deadline, DeadlineMessage, Saga, SagaContext, SagaEffect, SagaId, SagaManagerProps,
    /// # };
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
    /// # impl DeadlineMessage for Reminder {
    /// #     const TYPE_NAME: &'static str = "reminder";
    /// #     const CODEC_NAME: &'static str = "reminder-codec";
    /// #     type Codec = ReminderCodec;
    /// # }
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
    /// // `Observed`'s `Deadline` is `NoDeadline`, not `HasDeadline<Reminder>`,
    /// // so the `where` clause below has nothing to satisfy this call with.
    /// SagaManagerProps::<Undeclared>::new(|components| Undeclared {
    ///     deadline: components.deadline(),
    /// });
    /// ```
    pub fn deadline<M: DeadlineMessage>(&self) -> Deadline<M>
    where
        S::Subscribe: SagaSubscribe<Deadline = HasDeadline<M>>,
    {
        Deadline::new(self.pending_deadlines.clone())
    }
}
