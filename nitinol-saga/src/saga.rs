//! The `Saga` trait — user-facing process-manager abstraction.

use async_trait::async_trait;

use nitinol_eventsource::Event;

use crate::context::SagaContext;
use crate::delivery::SagaDelivery;
use crate::effect::SagaEffect;
use crate::id::SagaId;
use crate::subscribe::SagaSubscribe;

/// Event-sourced process manager: the sole writer of its own stream, deciding
/// what happens next for each input it declares. See the crate's
/// [mental model](crate#mental-model).
///
/// - Every input, events from the aggregates it observes and deadlines of its
///   own, is declared in one enum, [`Saga::Subscribe`]. For an observed input
///   the framework calls [`Saga::correlate`] to find the instance, then
///   [`Saga::handle`] on it; both `match` the whole alphabet, so a forgotten
///   reaction is a compile error.
/// - `handle` answers a [`SagaEffect`]: persist the saga's own events, set or
///   cancel a deadline, terminate, or do nothing.
/// - [`Saga::apply`] folds the saga's own events into `self`, during replay and
///   after each append. It is synchronous and does no I/O, so replay is
///   deterministic; the implementing type is the whole of the state.
/// - [`Saga::dispatch`] maps each persisted decision to the commands a delivery
///   host carries out.
/// - `correlate` and `dispatch` are associated functions because they are
///   answered without an instance: before one is found, and by a host that
///   keeps none resident.
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
    /// another from progressing.  There is no total order across sources.
    type Subscribe: SagaSubscribe;

    /// The saga's own internal events — the stream the delivery host consumes,
    /// and what replay reconstructs state and consumed positions from.
    type Event: Event;

    /// Domain-level error type produced by [`Saga::handle`].
    ///
    /// A refused input is reported through `tracing` and settled by its
    /// consumed position alone — see [`Saga::handle`] for what that means for
    /// redelivery.  This type exists to keep the signature symmetrical with
    /// `Decider::Rejection` and to give the implementation a place to surface
    /// diagnostics.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Derive the instance an observed input belongs to.
    ///
    /// Answered from the input alone, before any instance exists to ask. The
    /// input reaches [`Saga::handle`] only on the instance whose [`SagaId`] this
    /// returns; `None` means the input is ignored, which is not a failure and
    /// records no dead letter.
    ///
    /// The `#[subscribe(deadline)]` arm returns `None`: a firing is addressed to
    /// the instance that set it and never asks this question.
    ///
    /// Match the whole alphabet, so a source with no correlation rule is a
    /// **compile error**:
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
    /// is addressed to.
    ///
    /// [`Saga::apply`] says what a decision means to the saga; this says what it
    /// means for everybody else. A delivery host answers it from the store with
    /// no saga resident, so it takes only the event: the target and the
    /// operation id the receiver recognises a repeat by belong in the decision.
    ///
    /// Return zero or more; the host delivers them one at a time in this order
    /// and finishes with the decision only once every one has landed. The
    /// default returns none.
    fn dispatch(event: &Self::Event) -> Vec<SagaDelivery> {
        let _ = event;
        Vec::new()
    }

    /// React to one input: an observed event that [`Saga::correlate`] maps to
    /// this instance and that it has not already consumed, or a deadline of this
    /// instance that fires.
    ///
    /// - The returned effect is persisted together with the input's position, by
    ///   the last append the effect needs, so the position is never durable
    ///   before the decision. A record at or below the position the saga's own
    ///   stream records for its source is dropped before it reaches here.
    /// - An `Err` on an observed record writes its position alone, and the record
    ///   does not come back. An effect that writes nothing else still appends the
    ///   position, so a no-op decision is not redelivered either.
    /// - A record whose bytes fail to decode never reaches here: it is reported
    ///   and skipped, and nothing is written on the saga's stream for it.
    /// - Until the settling append lands, the record is not consumed and comes
    ///   back on the next poll. The `Ended` fact is withheld the same way when an
    ///   earlier append the effect needed did not land, so the saga is never
    ///   terminated around a decision no store holds. For a decision the saga
    ///   also stays running, so the redelivery retries it, `End` included, and a
    ///   later manager reads the record again from the source's offset.
    /// - A position names both its stream and its declared source, so progress
    ///   on one never covers a record of another.
    /// - A firing has no position, so nothing it decides records one; it is
    ///   settled by its `fired` fact, an `Err` included, as stated on
    ///   [`Deadline`](crate::Deadline).
    ///
    /// What a delivery host carries, and how often, is stated on
    /// [`SagaDeliveryProps`](crate::SagaDeliveryProps). Match the whole
    /// alphabet, so a source with no reaction is a **compile error**:
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
