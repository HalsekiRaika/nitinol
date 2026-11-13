use async_trait::async_trait;
use nitinol_contract::Event;

use crate::projection::context::ProjectionContext;

/// A projection handler for event type `E` with transaction type `Tx`.
///
/// - One projector type may implement `Projector<E>` for several event types.
///   `Tx` defaults to `()` and matches `OffsetStore::Tx`; under `ExactlyOnce`,
///   `ctx.tx()` is the transaction.
/// - A child poller catches up from the last saved offset, then keeps polling
///   live, and stops with the projector. Events arrive in sequence order, and
///   each read of at most [`crate::ProjectorProps::with_batch_limit`] records
///   is settled as one batch (see [`nitinol_persistence::store::DeliveryMode`]);
///   [`ProjectionContext::current_sequence`] is still the event's own sequence.
/// - Under `AtLeastOnce` (the default) an event can arrive more than once, so
///   `project` must be idempotent. Otherwise use `ExactlyOnce` with a
///   [`crate::TxProvider`]: a failed batch is rolled back and redelivered from
///   its first event rather than partially applied.
/// - Under `AtLeastOnce` and `ExactlyOnce` a failed event is redelivered on every
///   poll and holds every event behind it; one that can never succeed stalls the
///   projection, with a warning per poll interval and an offset that stops
///   moving, until `project` returns `Ok` for it. `AtMostOnce` skips it instead.
#[async_trait]
pub trait Projector<E: Event, Tx = ()>: Send + Sync + 'static {
    /// The error [`project`](Self::project) returns when it cannot apply an
    /// event.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Applies `event` to the read model.
    async fn project(
        &mut self,
        event: E,
        ctx: &mut ProjectionContext<'_, Tx>,
    ) -> Result<(), Self::Error>;
}
