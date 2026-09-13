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
///
/// # A failed event holds the ones behind it
///
/// Under `AtLeastOnce` and `ExactlyOnce`, an event whose application did not
/// succeed is not overtaken: the subscription stays on it and redelivers it on
/// every poll until it succeeds. An event that can never succeed — a payload
/// its codec cannot decode, a row the read model will always reject — therefore
/// stops that projection's progress entirely, retrying and logging a warning
/// once per poll interval. Nothing behind it is applied, and nothing is
/// silently dropped.
///
/// That is the deliberate price of not skipping failures. A projection stalled
/// this way is visible in the repeated warnings and in a checkpoint that stops
/// moving; recovering it means making the event succeed — fixing the projector,
/// or having `project` recognise the record as one it chooses to accept and
/// return `Ok`. `AtMostOnce` is the mode that trades the event away instead:
/// there a failed handler is left behind and the subscription moves on.
#[async_trait]
pub trait Projector<E: Event, Tx = ()>: Send + Sync + 'static {
    type Error: std::error::Error + Send + Sync + 'static;

    async fn project(
        &mut self,
        event: E,
        ctx: &mut ProjectionContext<'_, Tx>,
    ) -> Result<(), Self::Error>;
}
