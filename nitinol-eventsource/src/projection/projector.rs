use async_trait::async_trait;
use nitinol_contract::Event;

use crate::projection::context::ProjectionContext;

/// A projection handler for event type `E` with transaction type `Tx`.
///
/// A single projector type `P` may implement `Projector<E>` for multiple event
/// types, enabling a single process to consume events from several aggregates.
///
/// `&mut self` allows stateful projectors (e.g., those holding a DB connection).
///
/// The `Tx` type parameter defaults to `()` and matches the `CheckpointStore::Tx`
/// associated type.  For ExactlyOnce delivery with a real database backend,
/// implement `Projector<E, DbTx>` and use `ctx.tx()` to access the transaction.
///
/// # Catch-up and live delivery
///
/// A projector process spawns a runtime child `DirectPollerProcess` that
/// performs catch-up from the last saved checkpoint and then continues with live
/// polling.  The child's lifetime is bound to the projector process; when the
/// projector stops the runtime cascade-stops the poller.  Events are handed to
/// `project` in sequence order.
///
/// # Idempotency
///
/// Under `AtLeastOnce` delivery (the default), `project` must be idempotent.
/// The same event can arrive more than once — for example when a process
/// restarts between handling an event and saving its checkpoint.
///
/// For non-idempotent read-model updates (e.g., incrementing a counter), use
/// `ExactlyOnce` delivery with a [`crate::TxProvider`] configured on the
/// projector. The framework then manages `begin → project → checkpoint save →
/// commit`, rolling back the transaction if either step fails, so the event
/// is redelivered rather than partially applied.
#[async_trait]
pub trait Projector<E: Event, Tx = ()>: Send + Sync + 'static {
    type Error: std::error::Error + Send + Sync + 'static;

    async fn project(
        &mut self,
        event: E,
        ctx: &mut ProjectionContext<'_, Tx>,
    ) -> Result<(), Self::Error>;
}
