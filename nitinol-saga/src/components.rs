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

use nitinol_eventsource::system::ContingentSystem;

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
    host: ContingentSystem,
    _saga: PhantomData<fn() -> S>,
}

impl<S: Saga> SagaComponents<S> {
    /// Resolve the components of one incarnation.
    ///
    /// Called once per incarnation, so the cell the process writes and the one
    /// the saga's [`Deadline`] reads are the same cell — and a restarted
    /// incarnation starts from an empty one, which replay then fills.
    pub(crate) fn new(pending_deadlines: PendingDeadlines, host: ContingentSystem) -> Self {
        Self {
            pending_deadlines,
            host,
            _saga: PhantomData,
        }
    }

    /// The system hosting this saga, whichever entry point started it.
    ///
    /// A saga that has to tell an aggregate resolves the reference from here
    /// rather than from a clone of the system captured outside the spawn. That
    /// includes an aggregate whose [`Recover`](nitinol_eventsource::Aggregate::Recover)
    /// is [`Latest`](nitinol_eventsource::Latest), which recovers from the host's
    /// snapshot slot; where the store holds none, every dispatch to it fails with
    /// [`AskError::Activation`](nitinol_eventsource::AskError::Activation)
    /// ([`TellError::Activation`](nitinol_eventsource::TellError::Activation) for
    /// a `tell`), which `retryability` reads as permanent.
    pub fn system(&self) -> &ContingentSystem {
        &self.host
    }

    /// Ask for this saga's own time, with deadlines carrying the message `M`.
    ///
    /// Asking is the declaration: the host resolves the resident timer core for
    /// a saga whose alphabet has a `#[subscribe(deadline)]` variant naming `M`,
    /// and only such a saga can call this. Asking for `M` otherwise is a
    /// **compile error**:
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
