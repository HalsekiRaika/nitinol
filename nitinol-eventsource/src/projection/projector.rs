use async_trait::async_trait;

use crate::event::Event;
use crate::projection::context::ProjectionContext;

/// A projection handler for event type `E` with transaction type `Tx`.
///
/// A single projector type `P` may implement `Projector<E>` for multiple event
/// types, enabling a single process to consume events from several aggregates.
///
/// `&mut self` allows stateful projectors (e.g. those holding a DB connection).
///
/// The `Tx` type parameter defaults to `()` and matches the `CheckpointStore::Tx`
/// associated type.  For ExactlyOnce delivery with a real database backend,
/// implement `Projector<E, DbTx>` and use `ctx.tx()` to access the transaction.
#[async_trait]
pub trait Projector<E: Event, Tx = ()>: Send + Sync + 'static {
    type Error: std::error::Error + Send + Sync + 'static;

    async fn project(
        &mut self,
        event: E,
        ctx: &mut ProjectionContext<'_, Tx>,
    ) -> Result<(), Self::Error>;
}
