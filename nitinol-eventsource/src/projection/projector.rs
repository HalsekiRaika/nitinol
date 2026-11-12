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
/// The `Tx` type parameter defaults to `()` and matches the `OffsetStore::Tx`
/// associated type.  For ExactlyOnce delivery with a real database backend,
/// implement `Projector<E, DbTx>` and use `ctx.tx()` to access the transaction.
///
/// # Catch-up and live delivery
///
/// A projector process spawns a runtime child poller that performs catch-up
/// from the last saved offset and then continues with live polling.  The
/// child's lifetime is bound to the projector process; when the projector stops
/// the runtime cascade-stops the poller.  Events are handed to `project` in
/// sequence order.
///
/// What each read of the journal returned — at most
/// [`crate::ProjectorProps::with_batch_limit`] records — is settled as one
/// batch: the offset is saved once per batch, and the delivery mode decides
/// where in it (see [`nitinol_persistence::store::DeliveryMode`]).
/// [`ProjectionContext::current_sequence`] is still the sequence of the event
/// being projected, not of the batch.
///
/// # Idempotency
///
/// Under `AtLeastOnce` delivery (the default), `project` must be idempotent.
/// The same event can arrive more than once — for example when a process
/// restarts between handling an event and saving its offset.
///
/// For non-idempotent read-model updates (e.g., incrementing a counter), use
/// `ExactlyOnce` delivery with a [`crate::TxProvider`] configured on the
/// projector. The framework then manages `begin → project (every event of the
/// batch) → offset save → commit`, rolling back the transaction if any step
/// fails, so the batch is redelivered from its first event rather than
/// partially applied.
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
/// That is the cost of not skipping failures. A projection stalled
/// this way is visible in the repeated warnings and in an offset that stops
/// moving; recovering it means making the event succeed — fixing the projector,
/// or having `project` recognise the record as one it chooses to accept and
/// return `Ok`. `AtMostOnce` is the mode that trades the event away instead:
/// there a failed handler is left behind and the subscription moves on.
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
