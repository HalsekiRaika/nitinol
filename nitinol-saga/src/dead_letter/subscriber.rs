//! DLQ subscriber wiring via [`DurableSubscription`].
//!
//! `SagaProps::with_dead_letter_subscriber(proxy)` — or
//! `SagaManagerProps::with_dead_letter_subscriber(proxy)` for every instance a
//! manager spawns — registers a subscriber process
//! `P: Receive<DeadLetterEvent>`.  At spawn time the saga starts a
//! [`DurableSubscription`] direct poller for the subscriber over the saga's
//! **own** EventStore stream, decoding only dead-letter events.  The subscriber
//! catches up from the saga stream even if it was down when the dead letter
//! landed.
//!
//! Unlike the previous `DurableStream`-based approach, this wiring never
//! registers a shared `Stream<T>` process under a topic derived from the saga
//! id.  This means the same `ProcessSystem` can re-spawn the same saga id
//! repeatedly without topic collisions, so subscriber catchup keeps working
//! across saga re-spawns.
//!
//! The subscriber's process type `P` is erased into a [`DlqChildSpawn<Parent>`]
//! trait object so it never surfaces as a type parameter of
//! `SagaProps` / `SagaProcess`.  The DLQ direct poller is spawned as a
//! **child** of the saga from `SagaProcess::on_start`, so the saga stop's child
//! cascade automatically stops the poller.

use std::sync::Arc;

use futures_core::future::BoxFuture;
use nitinol_eventsource::{DurableSubscription, SequenceCursor, SystemEvent};
use nitinol_persistence::store::EventStore;
use nitinol_persistence::LoadedEvent;
use nitinol_runtime::ident::{Pid, ProcessName};
use nitinol_runtime::process::{Process, ProcessContext, ProcessProxy, Receive};

use crate::dead_letter::event::{is_dead_letter_event_type, DeadLetterEvent};
use crate::id::SagaId;

/// Object-safe interface for spawning the DLQ direct-poller as a **child** of
/// the calling process context.
///
/// Implemented by [`TypedDlqChildSpawn`] which captures the typed subscriber
/// proxy.  `SagaProcess<S>` stores `Option<Arc<dyn DlqChildSpawn<SagaProcess<S>>>>`
/// so the DLQ poller is started from `on_start` and its lifetime is bound to
/// the saga via child cascade — when the saga stops, the runtime
/// cascade-stops the DLQ poller automatically.
///
/// `P` is the parent process type; it does not need to receive
/// [`DeadLetterEvent`] itself — only the captured subscriber does.
pub(crate) trait DlqChildSpawn<P: Process>: Send + Sync {
    /// Spawn the DLQ direct-poller as a child of `ctx`'s process.
    ///
    /// The poller polls `store` at the stream key derived from `saga_id` and
    /// forwards dead-letter events to the captured subscriber proxy.
    fn spawn_as_child<'a>(
        &'a self,
        ctx: &'a mut ProcessContext<P>,
        saga_id: &'a SagaId,
        store: &'a Arc<dyn EventStore>,
    ) -> BoxFuture<'a, ()>;
}

/// Concrete implementation of [`DlqChildSpawn`] that captures a typed
/// subscriber proxy `ProcessProxy<Sub>`.
///
/// `Sub` is erased at the `DlqChildSpawn` trait boundary so it does not
/// propagate to `SagaProps` / `SagaProcess`.
struct TypedDlqChildSpawn<Sub> {
    subscriber: ProcessProxy<Sub>,
}

impl<P, Sub> DlqChildSpawn<P> for TypedDlqChildSpawn<Sub>
where
    P: Process,
    Sub: Process + Receive<DeadLetterEvent, Response = ()> + 'static,
{
    fn spawn_as_child<'a>(
        &'a self,
        ctx: &'a mut ProcessContext<P>,
        saga_id: &'a SagaId,
        store: &'a Arc<dyn EventStore>,
    ) -> BoxFuture<'a, ()> {
        let subscriber = self.subscriber.clone();
        let cursor = SequenceCursor::Stream {
            key: saga_id.as_str().to_owned(),
            after: 0,
        };
        let poller_name = dlq_poller_name(subscriber.pid(), saga_id);
        let sub_config = DurableSubscription::new(Arc::clone(store), dead_letter_transform);
        Box::pin(async move {
            sub_config
                .spawn_child_for(ctx, subscriber, cursor, poller_name)
                .await;
        })
    }
}

/// Construct a type-erased [`DlqChildSpawn`] for `subscriber`.
///
/// `P` is the parent process type (typically `SagaProcess<S>`); it does not
/// need to receive [`DeadLetterEvent`].  Only `Sub` needs that trait bound.
pub(crate) fn make_dlq_child_spawn<P, Sub>(
    subscriber: ProcessProxy<Sub>,
) -> Arc<dyn DlqChildSpawn<P>>
where
    P: Process,
    Sub: Process + Receive<DeadLetterEvent, Response = ()> + 'static,
{
    Arc::new(TypedDlqChildSpawn { subscriber })
}

/// Decode only dead-letter events off the saga stream; skip everything else
/// (domain / outbox / schedule) so the subscriber sees dead letters alone.
fn dead_letter_transform(loaded: LoadedEvent) -> Option<DeadLetterEvent> {
    if !is_dead_letter_event_type(loaded.event_type) {
        return None;
    }
    match DeadLetterEvent::decode(&loaded.payload) {
        Ok(event) => Some(event),
        Err(e) => {
            tracing::warn!(error = %e, "saga dead letter decode failed; skipping event");
            None
        }
    }
}

/// Build the poller name for a DLQ direct poller.
///
/// The name encodes both the subscriber pid and the saga id so that the same
/// subscriber process can be wired as a DLQ subscriber for multiple different
/// sagas simultaneously — each subscription gets its own direct poller and they
/// coexist without interfering.  Without the saga id component, registering the
/// same subscriber for saga B would stop the poller for saga A.
pub(crate) fn dlq_poller_name(subscriber_pid: Pid, saga_id: &SagaId) -> ProcessName {
    ProcessName::new(format!(
        "dlq-poller-{}-{}",
        subscriber_pid,
        saga_id.as_str()
    ))
}
