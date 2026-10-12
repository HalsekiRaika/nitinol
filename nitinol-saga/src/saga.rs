//! The `Saga` trait — user-facing process-manager abstraction.

use async_trait::async_trait;

use nitinol_eventsource::Event;

use crate::context::SagaContext;
use crate::delivery::SagaDelivery;
use crate::effect::SagaEffect;
use crate::id::SagaId;
use crate::subscribe::SagaSubscribe;

/// Event-sourced process manager.
///
/// A saga reacts to the inputs it declares — events emitted by the aggregates it
/// observes, and deadlines of its own — and produces a declarative
/// [`SagaEffect`] describing what should happen next: persist new saga events,
/// set or cancel a deadline, terminate, or do nothing.  What a decision means
/// for everybody else is the separate pure mapping [`Saga::dispatch`] answers,
/// and carrying it out belongs to a delivery host.
///
/// # One alphabet, one entry point
///
/// Everything a saga reacts to is declared as [`Saga::Subscribe`], and
/// everything reaches it through [`Saga::handle`].  A coordinator observes the
/// reply of whoever it addressed *and* the passing of its own deadline, so a
/// trait that named one upstream event type could not express the failure
/// handling a saga exists for.  Declaring the inputs as one enum is also what
/// makes a forgotten reaction a compile error rather than a silent gap:
/// [`Saga::correlate`] and [`Saga::handle`] both `match` on it.
///
/// # Why `apply` is synchronous and `handle` is async
///
/// `apply` is a pure state transition driven by the saga's own events during
/// replay — it does no I/O.  Keeping it synchronous mirrors
/// [`nitinol_eventsource::Aggregate::apply`] and makes replay deterministic
/// and trivially testable without a runtime.
///
/// `handle` is async because it represents the saga's reactive entry point:
/// the implementation may await proxies, codecs, or domain services to
/// produce its effect.
///
/// # The user never sees the runtime `Process` trait
///
/// `Saga` is the only trait the user implements.  The internal
/// `SagaProcess<S: Saga>` wrapper that adapts it to `nitinol_runtime::Process`
/// is `pub(crate)` and never appears in any signature exposed by this crate.
///
/// # Where the saga's state lives
///
/// A saga's state is the implementor itself: [`Saga::apply`] mutates `&mut
/// self`, so fields on the implementing type are the whole of the state.  The
/// trait carries no associated state type.
///
/// # Snapshotting is not part of this trait
///
/// A saga replays purely from its own event stream.  When snapshotting is
/// implemented it arrives as a separate opt-in extension trait
/// (`SagaSnapshotable`, symmetrical to
/// [`nitinol_eventsource::Snapshotable`]) carrying an associated snapshot type
/// plus its capture/restore pair — so a saga that does not snapshot never sees
/// the surface, and the ability is visible in the type system rather than
/// hidden behind a default that panics.
#[async_trait]
pub trait Saga: Send + Sync + 'static {
    /// The whole alphabet of inputs this saga reacts to.
    ///
    /// A user-defined enum carrying `#[derive(SagaSubscribe)]`: one newtype
    /// variant per external event type the saga observes, plus at most one
    /// `#[subscribe(deadline)]` variant naming the saga's own time.
    ///
    /// Each external variant is observed as a source of its own — its own
    /// subscription, its own cursor — so one source stalling does not stop
    /// another from progressing.  There is deliberately no total order across
    /// sources: it is not a promise that can be kept over several upstream
    /// streams in a distributed system, so it is not made.
    type Subscribe: SagaSubscribe;

    /// The saga's own internal events — the stream the delivery host consumes,
    /// and what replay reconstructs state and consumed positions from.
    type Event: Event;

    /// Domain-level error type produced by [`Saga::handle`].
    ///
    /// A refused input is reported through `tracing` and settled by its
    /// position alone — see [`Saga::handle`] for what that means for
    /// redelivery.  This type exists to keep the signature symmetrical with
    /// `Decider::Rejection` and to give the implementation a place to surface
    /// diagnostics.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Derive the identity of the business process instance an observed input
    /// belongs to — "how do I get found?".
    ///
    /// # Why this is an associated function
    ///
    /// The runtime has to decide whom an input belongs to *before* it has a
    /// saga to ask, so correlation cannot depend on instance state.  Taking no
    /// receiver states that in the type system: the answer is derived from the
    /// input alone, and the call site is `<S as Saga>::correlate(&input)`.
    ///
    /// # Why correlation, and not routing
    ///
    /// Correlation is the domain knowledge that maps an observed event to a
    /// process instance; routing is the runtime responsibility of carrying a
    /// message to wherever that instance lives.  This trait declares the former,
    /// so it travels with the saga type instead of leaking into every spawn
    /// site.  The latter stays with the runtime.
    ///
    /// # The deadline arm answers `None`
    ///
    /// A deadline's firing is addressed to the instance that set it and never
    /// travels the subscription path, so the framework never asks this question
    /// about one: the `#[subscribe(deadline)]` arm is unreachable here and
    /// returning `None` is what says so.
    ///
    /// # How the answer is used
    ///
    /// An observed event reaches [`Saga::handle`] only when the returned
    /// [`SagaId`] equals the id this instance was spawned with.  `None`, or a
    /// `Some` naming a different instance, means the event belongs to somebody
    /// else and is ignored silently — it is not a failure and records no dead
    /// letter.
    ///
    /// There is deliberately no default: correlation has no universal rule, and
    /// a defaulted `None` would silently discard every observed event.
    ///
    /// # A source with no correlation rule is a compile error
    ///
    /// Taking the whole alphabet is what makes the omission visible: the saga
    /// below observes two sources and answers for one, and the `match` is
    /// non-exhaustive.
    ///
    /// ```compile_fail
    /// # use async_trait::async_trait;
    /// # use bytes::Bytes;
    /// # use nitinol_eventsource::codec::Codec;
    /// # use nitinol_eventsource::Event;
    /// # use nitinol_persistence::{EventType, Family, TypeName};
    /// # use nitinol_saga::{Saga, SagaContext, SagaEffect, SagaId, SagaSubscribe};
    /// # struct DocCodec;
    /// # #[derive(Clone)] struct OrderPlaced;
    /// # #[derive(Clone)] struct StockReserved;
    /// # #[derive(Clone)] struct Decided;
    /// # impl Codec<OrderPlaced> for DocCodec {
    /// #     type Error = std::convert::Infallible;
    /// #     fn encode(_: &OrderPlaced) -> Result<Bytes, Self::Error> { Ok(Bytes::new()) }
    /// #     fn decode(_: &[u8]) -> Result<OrderPlaced, Self::Error> { Ok(OrderPlaced) }
    /// # }
    /// # impl Codec<StockReserved> for DocCodec {
    /// #     type Error = std::convert::Infallible;
    /// #     fn encode(_: &StockReserved) -> Result<Bytes, Self::Error> { Ok(Bytes::new()) }
    /// #     fn decode(_: &[u8]) -> Result<StockReserved, Self::Error> { Ok(StockReserved) }
    /// # }
    /// # impl Codec<Decided> for DocCodec {
    /// #     type Error = std::convert::Infallible;
    /// #     fn encode(_: &Decided) -> Result<Bytes, Self::Error> { Ok(Bytes::new()) }
    /// #     fn decode(_: &[u8]) -> Result<Decided, Self::Error> { Ok(Decided) }
    /// # }
    /// # impl Event for OrderPlaced {
    /// #     const EVENT_TYPE: EventType = EventType::new(Family::new("doc"), TypeName::new("OrderPlaced"));
    /// #     type Codec = DocCodec;
    /// # }
    /// # impl Event for StockReserved {
    /// #     const EVENT_TYPE: EventType = EventType::new(Family::new("doc"), TypeName::new("StockReserved"));
    /// #     type Codec = DocCodec;
    /// # }
    /// # impl Event for Decided {
    /// #     const EVENT_TYPE: EventType = EventType::new(Family::new("doc"), TypeName::new("Decided"));
    /// #     type Codec = DocCodec;
    /// # }
    /// # #[derive(SagaSubscribe)]
    /// # enum Observed { Order(OrderPlaced), Stock(StockReserved) }
    /// # struct TwoSourceSaga;
    /// #[async_trait]
    /// impl Saga for TwoSourceSaga {
    ///     type Subscribe = Observed;
    ///     type Event = Decided;
    ///     type Error = std::convert::Infallible;
    ///
    ///     fn correlate(input: &Observed) -> Option<SagaId> {
    ///         // `Observed::Stock` is unanswered, so this `match` does not compile
    ///         match input {
    ///             Observed::Order(_) => Some(SagaId::new("two-source")),
    ///         }
    ///     }
    ///
    ///     async fn handle(&mut self, input: Observed, _: &mut SagaContext)
    ///         -> Result<SagaEffect<Decided>, Self::Error>
    ///     {
    ///         match input {
    ///             Observed::Order(_) | Observed::Stock(_) => Ok(SagaEffect::None),
    ///         }
    ///     }
    /// }
    /// ```
    fn correlate(input: &Self::Subscribe) -> Option<SagaId>;

    /// Apply one of the saga's own events to the in-memory state.
    ///
    /// Called during replay (`on_start`) for every event in the saga's event
    /// stream, and again after each [`SagaEffect::Persist`] event is appended.
    ///
    /// The default does nothing, because a saga need not hold state: a
    /// translating saga whose every decision follows from its input alone has no
    /// state to fold its own events into, and obliging it to write an empty
    /// method would be a ceremony with no reader.  Its own event stream is kept
    /// regardless — that stream is what the delivery host consumes and what the
    /// consumed positions are folded out of, so it is not optional even where
    /// the fold into state is.
    fn apply(&mut self, event: Self::Event) {
        let _ = event;
    }

    /// Derive the commands one of the saga's own events implies, and whom each
    /// is addressed to — "what follows from having decided this?".
    ///
    /// The counterpart of [`Saga::apply`]: that one says what a decision means
    /// to the saga itself, this one says what it means for everybody else. Both
    /// are pure functions of the decision, and a delivery host is what carries
    /// what this one answers to its targets.
    ///
    /// # Why this is an associated function
    ///
    /// A delivery host keeps no saga resident. It reads decisions out of the
    /// store — including ones persisted by an incarnation that no longer
    /// exists, or by a different process entirely — so there is no `&self` for
    /// it to ask. Taking no receiver states that in the type system, the same
    /// way [`Saga::correlate`] does.
    ///
    /// # Why the event is the only argument
    ///
    /// Everything a delivery needs has to be in the decision: whom it is
    /// addressed to, and the operation id the receiver recognises a repeat by.
    /// A decision that left those out would oblige every consumer to reach back
    /// to where it came from, and the ones that cannot — a projection replaying
    /// an archive, a second host on a different subscription — could not act on
    /// it at all. So the mapping is handed the decision and nothing else, and
    /// the knowledge of delivery itself — polling, the cursor, the recorded
    /// progress, holding on a failure — stays with the framework.
    ///
    /// # Order, and how many
    ///
    /// Zero or more, in the order they are returned: a decision with no
    /// consequence outside the saga maps to none, and one whose consequences
    /// live on several streams maps to one delivery each. A host delivers them
    /// one at a time in this order, and finishes with the decision only once
    /// every one of them has landed.
    ///
    /// The default answers none, so a saga written without a mapping keeps
    /// compiling and delivers nothing. Its own stream is kept either way.
    fn dispatch(event: &Self::Event) -> Vec<SagaDelivery> {
        let _ = event;
        Vec::new()
    }

    /// The single reactive entry point.  Invoked for every observed event that
    /// [`Saga::correlate`] maps to this saga instance and that the instance has
    /// not already consumed, and for every deadline of this instance that fires.
    ///
    /// # Why one entry point for both
    ///
    /// A deadline is an input like any other — the saga's own promise coming
    /// back to it — so it arrives as the `#[subscribe(deadline)]` variant of the
    /// same alphabet rather than through a hook of its own.  One entry point is
    /// what lets a saga's reaction to "the reply never came" sit beside its
    /// reaction to the reply, in one `match`.
    ///
    /// What differs is only how the input got here: an observed event came
    /// through its source's subscription and carries a position to consume, a
    /// firing was addressed to this instance and carries none — which is why the
    /// consumed-position rules below speak of observed events alone, and why a
    /// firing has a settling row of its own (see *How a firing settles*).
    ///
    /// # What "already consumed" means
    ///
    /// Delivery is at-least-once, so the same upstream record can arrive more
    /// than once — redelivered after a restart, or twice over within one
    /// incarnation.  Whatever this method returns is persisted together with
    /// the position of the record it was decided from, by the append that
    /// closes out every append the returned effect required — the last one,
    /// when interpreting it reaches the store as more than one atomic batch —
    /// so the position is never durable before the decision it stands for is.
    /// The reverse can happen: a decision can be durable while its input's
    /// position is not.  A record at or below the position the saga's own
    /// stream records for that record's source is dropped before it reaches
    /// here, so de-duplicating the *input* is not this method's to do.
    ///
    /// A decision is not the only way a record becomes consumed. A record this
    /// method returns `Err` for produces no decision at all: it was seen, it
    /// produced nothing, and it must not come round again — so its position is
    /// written on its own, and is the only row such an input leaves. Once that
    /// append lands the record is consumed exactly as a decision's position
    /// would make it, and it does not reach this method again on redelivery.
    ///
    /// A record whose upstream bytes fail to decode is the one case that is
    /// neither decided nor settled: it is reported and skipped, its source's
    /// cursor advances past it, and nothing durable is written for it. A decode
    /// failure is deterministic and permanent — there is no typed input to
    /// correlate, so there is no instance it could be attributed to — so
    /// re-reading the record after a restart repeats the report rather than a
    /// decision.
    ///
    /// Where that guarantee stops:
    ///
    /// - A record decided by this method's own returned effect, or settled by
    ///   the position-only append this method's `Err` produces, is not
    ///   consumed until that settling append lands; if it does not, the record
    ///   reaches this method again on redelivery.  That holds for the
    ///   closing append too — including the durable `Ended` fact, which is
    ///   withheld the same way when an earlier append the same effect needed
    ///   did not land, so the saga is never durably terminated around a
    ///   decision no store holds.
    ///
    ///   *When* that redelivery comes differs between the two.  A refused
    ///   input whose position-only append failed holds its source's cursor, so
    ///   the record comes round again on the next poll.  A *decision* whose
    ///   batch never landed does not hold the cursor: the saga stays alive
    ///   instead — which is what lets its next input retry the decision, `End`
    ///   included — and the record comes round when a later incarnation reads
    ///   the source again, since nothing durable claims it.
    /// - An effect that writes nothing else still commits a consumed-position-only
    ///   append when the input it was decided from has a position not yet
    ///   durable — which is always the case for an observed event, since one
    ///   only ever reaches here when its position has not already landed — so a
    ///   no-op decision is not redelivered either. A deadline firing has no
    ///   position to consume, so nothing it decides records one.
    /// - A position names both the stream it was read from and the declared
    ///   source it settled, so progress on one upstream stream never covers a
    ///   record of another, and progress on one declared source never covers a
    ///   record of another source sharing its stream.
    ///
    /// # How a firing settles
    ///
    /// A firing carries no position, so the row that settles it is the `fired`
    /// fact of its own schedule — and that fact is written only once whatever
    /// this method returned for the firing is durable.  Same discipline, same
    /// reason: an input is not closed out before the decision it produced is in
    /// a store.  A decision that never became durable therefore leaves the
    /// deadline pending on the stream, and the next activation re-arms it — at
    /// once, its due time being past — and hands the firing here again.
    ///
    /// Firing is at-least-once as a result: a stop between the decision landing
    /// and the `fired` fact landing repeats the firing, so a deadline arm has to
    /// tolerate being handed the same one twice.  Nothing is retried while this
    /// incarnation lives; the retry is a later activation.
    ///
    /// A firing this method returns `Err` for is settled by the `fired` fact
    /// alone — the counterpart of the position-only append a refused observed
    /// record leaves — and so is one whose effect writes nothing, and one whose
    /// schedule fact names a message identity this alphabet no longer declares,
    /// which never reaches this method at all.  Two firings leave no `fired`
    /// fact: one the saga terminated on, since the `Ended` fact closes every
    /// deadline out, and one whose returned effect re-set the same
    /// [`TimerName`](crate::TimerName) and had that fact land, since replacing a
    /// schedule by name is itself what closes the old deadline.
    ///
    /// None of this makes what follows from a decision idempotent: what a
    /// delivery host carries, and how often, is stated on
    /// [`SagaDeliveryProps`](crate::SagaDeliveryProps).
    ///
    /// # A source with no reaction is a compile error
    ///
    /// The whole alphabet arrives here, so a source observed but never reacted to
    /// — the compensation nobody wrote — leaves this `match` non-exhaustive:
    ///
    /// ```compile_fail
    /// # use async_trait::async_trait;
    /// # use bytes::Bytes;
    /// # use nitinol_eventsource::codec::Codec;
    /// # use nitinol_eventsource::Event;
    /// # use nitinol_persistence::{EventType, Family, TypeName};
    /// # use nitinol_saga::{Saga, SagaContext, SagaEffect, SagaId, SagaSubscribe};
    /// # struct DocCodec;
    /// # #[derive(Clone)] struct OrderPlaced;
    /// # #[derive(Clone)] struct StockReserved;
    /// # #[derive(Clone)] struct Decided;
    /// # impl Codec<OrderPlaced> for DocCodec {
    /// #     type Error = std::convert::Infallible;
    /// #     fn encode(_: &OrderPlaced) -> Result<Bytes, Self::Error> { Ok(Bytes::new()) }
    /// #     fn decode(_: &[u8]) -> Result<OrderPlaced, Self::Error> { Ok(OrderPlaced) }
    /// # }
    /// # impl Codec<StockReserved> for DocCodec {
    /// #     type Error = std::convert::Infallible;
    /// #     fn encode(_: &StockReserved) -> Result<Bytes, Self::Error> { Ok(Bytes::new()) }
    /// #     fn decode(_: &[u8]) -> Result<StockReserved, Self::Error> { Ok(StockReserved) }
    /// # }
    /// # impl Codec<Decided> for DocCodec {
    /// #     type Error = std::convert::Infallible;
    /// #     fn encode(_: &Decided) -> Result<Bytes, Self::Error> { Ok(Bytes::new()) }
    /// #     fn decode(_: &[u8]) -> Result<Decided, Self::Error> { Ok(Decided) }
    /// # }
    /// # impl Event for OrderPlaced {
    /// #     const EVENT_TYPE: EventType = EventType::new(Family::new("doc"), TypeName::new("OrderPlaced"));
    /// #     type Codec = DocCodec;
    /// # }
    /// # impl Event for StockReserved {
    /// #     const EVENT_TYPE: EventType = EventType::new(Family::new("doc"), TypeName::new("StockReserved"));
    /// #     type Codec = DocCodec;
    /// # }
    /// # impl Event for Decided {
    /// #     const EVENT_TYPE: EventType = EventType::new(Family::new("doc"), TypeName::new("Decided"));
    /// #     type Codec = DocCodec;
    /// # }
    /// # #[derive(SagaSubscribe)]
    /// # enum Observed { Order(OrderPlaced), Stock(StockReserved) }
    /// # struct TwoSourceSaga;
    /// #[async_trait]
    /// impl Saga for TwoSourceSaga {
    ///     type Subscribe = Observed;
    ///     type Event = Decided;
    ///     type Error = std::convert::Infallible;
    ///
    ///     fn correlate(input: &Observed) -> Option<SagaId> {
    ///         match input {
    ///             Observed::Order(_) | Observed::Stock(_) => Some(SagaId::new("two-source")),
    ///         }
    ///     }
    ///
    ///     async fn handle(&mut self, input: Observed, _: &mut SagaContext)
    ///         -> Result<SagaEffect<Decided>, Self::Error>
    ///     {
    ///         // `Observed::Stock` has no reaction, so this `match` does not compile
    ///         match input {
    ///             Observed::Order(_) => Ok(SagaEffect::None),
    ///         }
    ///     }
    /// }
    /// ```
    async fn handle(
        &mut self,
        input: Self::Subscribe,
        ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error>;
}
