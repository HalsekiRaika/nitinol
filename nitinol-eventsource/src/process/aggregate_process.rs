use std::sync::Arc;

use futures_util::StreamExt;
use nitinol_persistence::store::{EventStore, SnapshotStore};
use nitinol_persistence::{AggregateId, AppendingEvent, LoadQuery};
use nitinol_runtime::process::{Process, ProcessContext, Receive};

use crate::aggregate::{Aggregate, SnapshotRestoreError};
use crate::context::Context;
use crate::decider::Decider;
use crate::Effect;
use crate::error::{AskHandlerError, EffectExecutionError, ExecHandlerError};
use crate::event::Event;
use crate::process::codec::EventCodec;
use crate::receive::Receive as EvtReceive;

// ---------------------------------------------------------------------------
// Type alias for the snapshot restoration callback
// ---------------------------------------------------------------------------

/// A heap-allocated, shareable function that restores an aggregate from a
/// snapshot payload.  Stored as `Option<SnapshotRestoreFn<A>>` so it is set
/// only when both `Snapshotable` and a snapshot store are in use.
pub(crate) type SnapshotRestoreFn<A> =
    Arc<dyn Fn(&[u8]) -> Result<A, SnapshotRestoreError> + Send + Sync>;

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
    pub(crate) event_store: Arc<dyn EventStore>,
    pub(crate) snapshot_store: Option<Arc<dyn SnapshotStore>>,
    pub(crate) codec: Arc<dyn EventCodec<A::Event>>,
    pub(crate) sequence: u64,
    /// Restores aggregate state from a snapshot payload.
    /// Set only when the aggregate implements `Snapshotable` and a snapshot
    /// store was provided via `AggregateProps::with_snapshot_store`.
    pub(crate) snapshot_restore: Option<SnapshotRestoreFn<A>>,
}

impl<A: Aggregate> Process for AggregateProcess<A> {
    async fn on_start(&mut self, _ctx: &mut ProcessContext) {
        if let (Some(restore_fn), Some(ss)) = (
            self.snapshot_restore.clone(),
            self.snapshot_store.clone(),
        ) {
            match ss.load_latest(&self.aggregate_id).await {
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
                    tracing::error!(error = %e, "snapshot load failed; proceeding without snapshot");
                }
            }
        }

        let query = LoadQuery {
            aggregate_id: Some(self.aggregate_id.clone()),
            from_aggregate_sequence: Some(self.sequence + 1),
            ..Default::default()
        };

        let stream = match self.event_store.load(query).await {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = %e, "event store load failed during replay");
                return;
            }
        };

        futures_util::pin_mut!(stream);
        while let Some(item) = stream.next().await {
            match item {
                Ok(loaded) => match self.codec.decode(loaded.event_type, loaded.payload) {
                    Ok(event) => {
                        self.state.apply(event);
                        self.sequence = loaded.sequence;
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "event decode failed; skipping event");
                    }
                },
                Err(e) => {
                    tracing::error!(error = %e, "event stream error; aborting replay");
                    break;
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
            self.event_store.as_ref(),
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
    event_store: &'a (impl EventStore + ?Sized),
    codec: &'a (impl EventCodec<A::Event> + ?Sized),
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
                    let payload = codec.encode(event).map_err(EffectExecutionError::Encode)?;
                    appending.push(AppendingEvent {
                        aggregate_id: aggregate_id.clone(),
                        sequence: next_sequence,
                        event_type: A::Event::EVENT_TYPE,
                        payload,
                        occurred_at: jiff::Timestamp::now(),
                    });
                }
                event_store
                    .append(aggregate_id, appending)
                    .await
                    .map_err(EffectExecutionError::Append)?;
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
                        run_effect(sub, state, aggregate_id, sequence, event_store, codec)
                            .await?;
                    all.append(&mut events);
                }
                Ok(all)
            }
        }
    })
}
