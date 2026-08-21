use std::borrow::Borrow;
use std::sync::Arc;

use futures_util::StreamExt;
use nitinol_persistence::error::AppendError;
use nitinol_persistence::store::EventStore;
use nitinol_persistence::{AggregateId, AppendingEvent, LoadQuery};
use nitinol_runtime::process::{Process, ProcessContext, Receive};

use crate::aggregate::Aggregate;
use crate::codec::ErasedCodec;
use crate::context::Context;
use crate::decider::Decider;
use crate::error::{AskHandlerError, CodecError, EffectExecutionError, ExecHandlerError};
use crate::event::Event;
use crate::process::snapshot_persistor::SnapshotPersistorProxy;
use crate::receive::Receive as EvtReceive;
use crate::Effect;

// Type alias for the snapshot restoration callback

/// A heap-allocated, shareable function that restores an aggregate from a
/// snapshot payload.  Stored as `Option<SnapshotRestoreFn<A>>` so it is set
/// only when both `Snapshotable` and a snapshot persistor are in use.
///
/// The function decodes the raw bytes to the snapshot domain value (via the
/// snapshot codec) and then calls `A::restore`.
pub(crate) type SnapshotRestoreFn<A> = Arc<dyn Fn(&[u8]) -> Result<A, CodecError> + Send + Sync>;

// Internal message wrappers
//
// The runtime dispatches messages by type.  Wrapping command types in `AskCmd`
// and query types in `ExecMsg` prevents accidental dispatch to the wrong impl
// and allows both `Decider<C>` and `eventsource::Receive<M>` to coexist on
// the same `AggregateProcess<A>` without overlapping trait bounds.

/// Routes a domain command through the `Decider<C>` path.
pub(crate) struct AskCmd<C>(pub(crate) C);

/// Routes a domain query through the `eventsource::Receive<M>` path.
pub(crate) struct ExecMsg<M>(pub(crate) M);

// AggregateProcess

/// The runtime `Process` that hosts an aggregate and handles commands / queries.
///
/// # The only writer, optimistically
///
/// An activation numbers its appends from the sequence it replayed to, which is
/// correct exactly while it is the only thing writing that stream.  It assumes
/// that rather than enforcing it: nothing here, and nothing in the resolve layer
/// above, prevents a second activation of the same aggregate from existing
/// elsewhere (R-2).
///
/// The assumption is checked where it matters.  The store rejects an append at a
/// sequence that is already taken, and that rejection *is* the detection: this
/// activation has been overtaken and everything it would decide from here is
/// derived from a state the stream has moved past.  It cannot repair that, so it
/// stops (C-1).  References to the aggregate outlive that stop and resolve it
/// again (R-5).
///
/// The one conflict that is not an overtake is a conflict on the stream's
/// genesis sequence, which means the aggregate already exists (C-2).
pub struct AggregateProcess<A: Aggregate> {
    pub(crate) state: A,
    pub(crate) aggregate_id: AggregateId,
    pub(crate) store: Arc<dyn EventStore>,
    pub(crate) snapshot_ref: Option<SnapshotPersistorProxy>,
    pub(crate) codec: Arc<dyn ErasedCodec<A::Event>>,
    pub(crate) sequence: u64,
    /// Restores aggregate state from a snapshot payload.
    /// Set only when the aggregate implements `Snapshotable` and a snapshot
    /// persistor was provided via `AggregateProps::with_snapshot_persistor`.
    pub(crate) snapshot_restore: Option<SnapshotRestoreFn<A>>,
}

impl<A: Aggregate> Process for AggregateProcess<A> {
    async fn on_start(&mut self, ctx: &mut ProcessContext<Self>) {
        if let (Some(restore_fn), Some(snapshot_proxy)) =
            (self.snapshot_restore.clone(), self.snapshot_ref.clone())
        {
            match snapshot_proxy.load_latest(self.aggregate_id.clone()).await {
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

        let query = LoadQuery {
            stream_key: Some(self.aggregate_id.as_str().to_owned()),
            from_stream_sequence: Some(self.sequence + 1),
            ..Default::default()
        };

        // A failed replay (load or stream error) leaves `self.sequence` at
        // whatever it was before this call — 0 for a fresh activation. Falling
        // through to the message loop with an unreplayed state would let C-2
        // (`*sequence == 0` on a genesis conflict means "already created") treat
        // this activation's own missing history as a redelivered creation and
        // answer a command as success even though it never replayed far enough
        // to know that.  Stopping here, the same way C-1 stops on an overtaken
        // append, ensures no command is ever decided from state this activation
        // never actually reached.
        let stream = match self.store.load(query).await {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = ?e, "event store load failed during replay");
                if let Err(e) = ctx.stop_self().await {
                    tracing::error!(error = %e, "stop-on-replay-failure could not be signalled");
                }
                return;
            }
        };

        futures_util::pin_mut!(stream);
        while let Some(item) = stream.next().await {
            let loaded = match item {
                Ok(ev) => ev,
                Err(e) => {
                    tracing::error!(error = ?e, "event store stream error during replay");
                    if let Err(e) = ctx.stop_self().await {
                        tracing::error!(error = %e, "stop-on-replay-failure could not be signalled");
                    }
                    return;
                }
            };
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

// Receive<AskCmd<C>>: command processing (Decider path)

impl<A, C> Receive<AskCmd<C>> for AggregateProcess<A>
where
    A: Aggregate + Decider<C>,
    A::Event: Clone,
    C: Send + Sync + 'static,
{
    type Response = Vec<A::Event>;
    type Error = AskHandlerError<<A as Decider<C>>::Rejection>;

    async fn recv(
        &mut self,
        msg: AskCmd<C>,
        process_ctx: &mut ProcessContext<Self>,
    ) -> Result<Self::Response, Self::Error> {
        let mut ctx = Context::new(self.aggregate_id.clone(), self.sequence);
        let effect = self
            .state
            .decide(msg.0, &mut ctx)
            .await
            .map_err(AskHandlerError::Rejection)?;

        let outcome = run_effect(
            effect,
            &mut self.state,
            &self.aggregate_id,
            &mut self.sequence,
            self.store.as_ref(),
            self.codec.as_ref(),
        )
        .await;

        // C-1. A conflict that reaches here is not a creation — `run_effect`
        // answers those as "already created" (C-2) — so the stream has been
        // written by someone else since this activation replayed it.  Stopping is
        // the whole response: reloading and retrying would only re-enter the race
        // this activation has already lost.
        if let Err(EffectExecutionError::Append(AppendError::SequenceConflict(_))) = &outcome {
            // Signalled explicitly rather than left to the supervision strategy,
            // because stopping is a property of the conflict: no strategy may
            // resume or restart this activation into writing the stream again.
            if let Err(e) = process_ctx.stop_self().await {
                tracing::error!(error = %e, "stop-on-conflict could not be signalled");
            }
        }

        outcome.map_err(AskHandlerError::Effect)
    }
}

// Receive<ExecMsg<M>>: query processing (eventsource::Receive path)

impl<A, M> Receive<ExecMsg<M>> for AggregateProcess<A>
where
    A: Aggregate + EvtReceive<M>,
    M: Send + Sync + 'static,
{
    type Response = <A as EvtReceive<M>>::Response;
    type Error = ExecHandlerError<<A as EvtReceive<M>>::Error>;

    async fn recv(
        &mut self,
        msg: ExecMsg<M>,
        _ctx: &mut ProcessContext<Self>,
    ) -> Result<Self::Response, Self::Error> {
        let mut ctx = Context::new(self.aggregate_id.clone(), self.sequence);
        self.state
            .recv(msg.0, &mut ctx)
            .await
            .map_err(ExecHandlerError::Domain)
    }
}

// Effect executor for AggregateProcess
//
// Handles Persist (append + apply), Apply (apply only), Side (fire-and-forget),
// Sequence (ordered execution), and None (no-op).

fn run_effect<'a, A: Aggregate>(
    effect: Effect<A::Event>,
    state: &'a mut A,
    aggregate_id: &'a AggregateId,
    sequence: &'a mut u64,
    store: &'a dyn EventStore,
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
                        sequence: next_sequence,
                        event_type: event.variant(),
                        payload,
                        occurred_at: jiff::Timestamp::now(),
                    });
                }
                match store.append(aggregate_id.borrow(), appending).await {
                    Ok(_) => {}
                    // C-2 / OCC-2. Appending from sequence zero is a creation, so
                    // a conflict there says the aggregate already exists — the
                    // expected answer to a creation command redelivered under
                    // at-least-once, not a failure.  Nothing was written, so
                    // nothing is applied and the counter does not move: a later
                    // command must address the genesis sequence again rather than
                    // write into a stream this activation never replayed.
                    Err(AppendError::SequenceConflict(_)) if *sequence == 0 => return Ok(vec![]),
                    Err(e) => return Err(EffectExecutionError::Append(e)),
                }
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
                        run_effect(sub, state, aggregate_id, sequence, store, codec).await?;
                    all.append(&mut events);
                }
                Ok(all)
            }
        }
    })
}
