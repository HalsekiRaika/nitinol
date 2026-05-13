use std::sync::Arc;

use nitinol_persistence::{AggregateId, AppendingEvent, LoadQuery};
use nitinol_runtime::process::{Process, ProcessContext, Receive};

use crate::aggregate::Aggregate;
use crate::codec::ErasedCodec;
use crate::context::Context;
use crate::decider::Decider;
use crate::error::{AskHandlerError, CodecError, EffectExecutionError, ExecHandlerError, PersistorUnreachableKind};
use crate::event::Event;
use crate::Effect;
use crate::process::persistence::{AppendEvents, EventPersistorProxy, SnapshotPersistorProxy};
use crate::receive::Receive as EvtReceive;

// ---------------------------------------------------------------------------
// Type alias for the snapshot restoration callback
// ---------------------------------------------------------------------------

/// A heap-allocated, shareable function that restores an aggregate from a
/// snapshot payload.  Stored as `Option<SnapshotRestoreFn<A>>` so it is set
/// only when both `Snapshotable` and a snapshot persistor are in use.
///
/// The function decodes the raw bytes to the snapshot domain value (via the
/// snapshot codec) and then calls `A::restore`.
pub(crate) type SnapshotRestoreFn<A> =
    Arc<dyn Fn(&[u8]) -> Result<A, CodecError> + Send + Sync>;

// ---------------------------------------------------------------------------
// Internal message wrappers
//
// The runtime dispatches messages by type.  Wrapping command types in `AskCmd`
// and query types in `ExecMsg` prevents accidental dispatch to the wrong impl
// and allows both `Decider<C>` and `eventsource::Receive<M>` to coexist on
// the same `AggregateProcess<A>` without overlapping trait bounds.
// ---------------------------------------------------------------------------

/// Routes a domain command through the `Decider<C>` path.
pub(crate) struct AskCmd<C>(pub(crate) C);

/// Routes a domain query through the `eventsource::Receive<M>` path.
pub(crate) struct ExecMsg<M>(pub(crate) M);

// ---------------------------------------------------------------------------
// AggregateProcess
// ---------------------------------------------------------------------------

/// The runtime `Process` that hosts an aggregate and handles commands / queries.
pub struct AggregateProcess<A: Aggregate> {
    pub(crate) state: A,
    pub(crate) aggregate_id: AggregateId,
    pub(crate) event_ref: EventPersistorProxy,
    pub(crate) snapshot_ref: Option<SnapshotPersistorProxy>,
    pub(crate) codec: Arc<dyn ErasedCodec<A::Event>>,
    pub(crate) sequence: u64,
    /// Restores aggregate state from a snapshot payload.
    /// Set only when the aggregate implements `Snapshotable` and a snapshot
    /// persistor was provided via `AggregateProps::with_snapshot_persistor`.
    pub(crate) snapshot_restore: Option<SnapshotRestoreFn<A>>,
}

impl<A: Aggregate> Process for AggregateProcess<A> {
    async fn on_start(&mut self, _ctx: &mut ProcessContext) {
        // Restore from snapshot if both a restore function and snapshot persistor are present.
        if let (Some(restore_fn), Some(snapshot_proxy)) = (
            self.snapshot_restore.clone(),
            self.snapshot_ref.clone(),
        ) {
            match snapshot_proxy
                .load_latest(self.aggregate_id.clone())
                .await
            {
                Ok(Some(snapshot)) => match restore_fn(&snapshot.payload) {
                    Ok(restored) => {
                        self.state = restored;
                        self.sequence = snapshot.sequence;
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "snapshot decode failed; restoring from default state");
                    }
                },
                Ok(None) => {}
                Err(e) => {
                    tracing::error!(error = ?e, "snapshot load failed; proceeding without snapshot");
                }
            }
        }

        // Replay events from the event persistor starting after the current sequence.
        let query = LoadQuery {
            aggregate_id: Some(self.aggregate_id.clone()),
            from_aggregate_sequence: Some(self.sequence + 1),
            ..Default::default()
        };

        let events = match self
            .event_ref
            .load(query)
            .await
        {
            Ok(evts) => evts,
            Err(e) => {
                tracing::error!(error = ?e, "event store load failed during replay");
                return;
            }
        };

        for loaded in events {
            match self.codec.decode(&loaded.payload) {
                Ok(event) => {
                    self.state.apply(event);
                    self.sequence = loaded.sequence;
                }
                Err(e) => {
                    tracing::error!(error = %e, "event decode failed; skipping event");
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Receive<AskCmd<C>>: command processing (Decider path)
// ---------------------------------------------------------------------------

impl<A, C> Receive<AskCmd<C>> for AggregateProcess<A>
where
    A: Aggregate + Decider<C>,
    A::Event: Clone,
    C: Send + Sync + 'static,
{
    type Response = Vec<A::Event>;
    type Error = AskHandlerError<<A as Decider<C>>::Rejection>;

    async fn recv(&mut self, msg: AskCmd<C>, _ctx: &mut ProcessContext) -> Result<Self::Response, Self::Error> {
        let mut ctx = Context::new(self.aggregate_id.clone(), self.sequence);
        let effect = self
            .state
            .decide(msg.0, &mut ctx)
            .await
            .map_err(AskHandlerError::Rejection)?;

        run_effect(
            effect,
            &mut self.state,
            &self.aggregate_id,
            &mut self.sequence,
            &self.event_ref,
            self.codec.as_ref(),
        )
        .await
        .map_err(AskHandlerError::Effect)
    }
}

// ---------------------------------------------------------------------------
// Receive<ExecMsg<M>>: query processing (eventsource::Receive path)
// ---------------------------------------------------------------------------

impl<A, M> Receive<ExecMsg<M>> for AggregateProcess<A>
where
    A: Aggregate + EvtReceive<M>,
    M: Send + Sync + 'static,
{
    type Response = <A as EvtReceive<M>>::Response;
    type Error = ExecHandlerError<<A as EvtReceive<M>>::Error>;

    async fn recv(&mut self, msg: ExecMsg<M>, _ctx: &mut ProcessContext) -> Result<Self::Response, Self::Error> {
        let mut ctx = Context::new(self.aggregate_id.clone(), self.sequence);
        self.state
            .recv(msg.0, &mut ctx)
            .await
            .map_err(ExecHandlerError::Domain)
    }
}

// ---------------------------------------------------------------------------
// Effect executor for AggregateProcess
//
// Handles Persist (append + apply), Apply (apply only), Side (fire-and-forget),
// Sequence (ordered execution), and None (no-op).
// ---------------------------------------------------------------------------

fn run_effect<'a, A: Aggregate>(
    effect: Effect<A::Event>,
    state: &'a mut A,
    aggregate_id: &'a AggregateId,
    sequence: &'a mut u64,
    event_ref: &'a EventPersistorProxy,
    codec: &'a dyn ErasedCodec<A::Event>,
) -> futures_core::future::BoxFuture<'a, Result<Vec<A::Event>, EffectExecutionError>>
where
    A::Event: Clone,
{
    Box::pin(async move {
        match effect {
            Effect::None => Ok(vec![]),

            Effect::Persist(events) => {
                let mut next_sequence = *sequence;
                let mut appending = Vec::with_capacity(events.len());
                for event in &events {
                    next_sequence += 1;
                    let payload = codec.encode(event).map_err(EffectExecutionError::Codec)?;
                    appending.push(AppendingEvent {
                        aggregate_id: aggregate_id.clone(),
                        sequence: next_sequence,
                        event_type: A::Event::EVENT_TYPE,
                        payload,
                        occurred_at: jiff::Timestamp::now(),
                    });
                }
                event_ref
                    .0
                    .ask(AppendEvents {
                        aggregate_id: aggregate_id.clone(),
                        events: appending,
                    })
                    .await
                    .map_err(map_append_ask_err)?;
                // Commit the sequence counter only after a successful append.
                *sequence = next_sequence;
                let returned = events.clone();
                for event in events {
                    state.apply(event);
                }
                Ok(returned)
            }

            Effect::Apply(events) => {
                let returned = events.clone();
                for event in events {
                    state.apply(event);
                }
                Ok(returned)
            }

            Effect::Side(side) => {
                tokio::spawn(async move {
                    if let Err(e) = side.execute().await {
                        tracing::warn!(error = %e, "side effect failed");
                    }
                });
                Ok(vec![])
            }

            Effect::Sequence(effects) => {
                let mut all = Vec::new();
                for sub in effects {
                    let mut events =
                        run_effect(sub, state, aggregate_id, sequence, event_ref, codec)
                            .await?;
                    all.append(&mut events);
                }
                Ok(all)
            }
        }
    })
}

fn map_append_ask_err(
    e: nitinol_runtime::error::AskError<nitinol_persistence::error::AppendError>,
) -> EffectExecutionError {
    match e {
        nitinol_runtime::error::AskError::Handler(e) => EffectExecutionError::Append(e),
        nitinol_runtime::error::AskError::DeadLetter { destination } => {
            EffectExecutionError::PersistorUnreachable(
                PersistorUnreachableKind::DeadLetter { destination },
            )
        }
        nitinol_runtime::error::AskError::ReplyDropped => {
            EffectExecutionError::PersistorUnreachable(PersistorUnreachableKind::ReplyDropped)
        }
    }
}
