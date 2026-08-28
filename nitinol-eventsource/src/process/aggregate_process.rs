use std::borrow::Borrow;
use std::sync::Arc;

use futures_util::StreamExt;
use nitinol_contract::{Aggregate, Decider, Decision, Event, Query};
use nitinol_persistence::error::AppendError;
use nitinol_persistence::store::EventStore;
use nitinol_persistence::{AggregateId, AppendingEvent, LoadQuery};
use nitinol_runtime::process::{Process, ProcessContext, Receive};

use crate::codec::ErasedCodec;
use crate::error::{AskHandlerError, CodecError, ExecHandlerError, PersistError};
use crate::process::snapshot_persistor::SnapshotPersistorProxy;

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
// and `TellCmd` and query types in `ExecMsg` prevents accidental dispatch to the
// wrong impl and allows both `Decider<C>` and `contract::Query<M>` to coexist on
// the same `AggregateProcess<A>` without overlapping trait bounds.
//
// A command carries the same decision either way, but the two dispatch paths
// owe the caller different things (L-5) — one delivers the output, the other has
// nobody to deliver it to and must report a refusal instead of dropping it — so
// each is its own wrapper and its own handler rather than one handler guessing
// which path it is on.

/// Routes a domain command through the `Decider<C>` path, answering with the
/// decision's output.
pub(crate) struct AskCmd<C>(pub(crate) C);

/// Routes a domain command through the `Decider<C>` path with nobody waiting for
/// the answer.
pub(crate) struct TellCmd<C>(pub(crate) C);

/// Routes a domain query through the `contract::Query<M>` path.
pub(crate) struct ExecMsg<M>(pub(crate) M);

// AggregateProcess

/// The runtime `Process` that hosts an aggregate and handles commands / queries.
///
/// # The only writer, optimistically
///
/// An activation numbers its appends from the sequence it replayed to, which is
/// correct exactly while it is the only thing writing that stream.  It assumes
/// that rather than enforcing it: neither this process nor the resolve layer
/// above it prevents a second activation of the same aggregate from existing
/// elsewhere in a cluster.  The assumption is checked by the store, which
/// rejects an append at a sequence that is already taken.
///
/// # The conflict contract
///
/// How this process answers such a rejection is the contract below.  Each clause
/// carries a stable label (`C-1`, `C-2`) shared with the executable record named
/// beneath it.  The labels are local to this module and to that record; code
/// elsewhere states what it needs in its own words.
///
/// # C-1: an overtaken writer stops
///
/// A [`SequenceConflict`](AppendError::SequenceConflict) on a **non-genesis**
/// sequence is the detection that this activation is no longer the only writer
/// of its stream.  Everything it would decide from here is derived from a state
/// the stream has already moved past, and it cannot repair that: reloading and
/// retrying would only re-enter the race it has already lost.
///
/// So it stops, immediately and unconditionally.  Stopping is signalled here
/// rather than left to a supervision strategy, because it is a property of the
/// conflict itself — no strategy may resume or restart this activation back into
/// writing the stream.
///
/// The same reasoning covers a failed replay.  An activation that could not read
/// its own history has not reached the state it would decide from, so it stops
/// on that too rather than continuing from an unreplayed state.
///
/// References to the aggregate outlive the stop and resolve it again, so a
/// caller sees a transient failure rather than a dead reference.
///
/// Fixed by `non_genesis_conflict_stops_the_losing_writer`,
/// `replay_failure_does_not_masquerade_as_a_genesis_conflict` and
/// `replay_decode_failure_does_not_masquerade_as_a_genesis_conflict` in
/// `nitinol-eventsource/tests/aggregate_conflict.rs`.
///
/// # C-2: a genesis conflict means the aggregate already exists
///
/// The one conflict that is not an overtake is a conflict on the stream's
/// **genesis** sequence — an append made from sequence zero, which is a
/// creation.  A conflict there says the aggregate has already been created,
/// which is the expected shape of a creation command redelivered under
/// at-least-once semantics rather than a fault of this activation.  So the
/// activation lives on; C-1 does not apply.
///
/// What the caller is told is that collision itself
/// ([`AskError::AlreadyCreated`](crate::AskError::AlreadyCreated)), not a
/// success.  No decision was reached — the events never landed — so there is no
/// output to deliver, and an interpreter that reported success would have to
/// invent one and let the caller believe it created what someone else did
/// (L-7).  Whether a redelivered creation is a duplicate to be ignored, a
/// conflict to be surfaced or a race to be retried is the consumer's judgement,
/// and it needs to see the collision to make it.
///
/// Nothing was written, so nothing is applied and the sequence counter does not
/// move.  A later command must address the genesis sequence again rather than
/// write into a stream this activation never replayed.
///
/// This reading depends on the activation having replayed far enough to know
/// its own sequence is zero, which is why C-1 stops a failed replay instead of
/// letting an unread history masquerade as a redelivered creation.
///
/// Fixed by `genesis_conflict_is_answered_as_already_created`,
/// `genesis_conflict_leaves_the_writer_alive_and_unchanged` and
/// `genesis_conflict_does_not_advance_the_writer_sequence` in
/// `nitinol-eventsource/tests/aggregate_conflict.rs`.  The store-side half —
/// that the conflict is returned at all, and that the first write survives it —
/// belongs to
/// [`EventStore::append`](nitinol_persistence::store::EventStore::append).
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
                    tracing::error!(error = %e, "event decode failed during replay");
                    if let Err(e) = ctx.stop_self().await {
                        tracing::error!(error = %e, "stop-on-replay-failure could not be signalled");
                    }
                    return;
                }
            }
        }
    }
}

// Receive<AskCmd<C>>: command processing with a caller waiting (Decider path)

impl<A, C> Receive<AskCmd<C>> for AggregateProcess<A>
where
    A: Aggregate + Decider<C>,
    C: Send + Sync + 'static,
    <A as Decider<C>>::Output: Send + 'static,
    <A as Decider<C>>::Rejection: std::error::Error + Send + Sync + 'static,
{
    type Response = <A as Decider<C>>::Output;
    type Error = AskHandlerError<<A as Decider<C>>::Rejection>;

    async fn recv(
        &mut self,
        msg: AskCmd<C>,
        process_ctx: &mut ProcessContext<Self>,
    ) -> Result<Self::Response, Self::Error> {
        match self.state.decide(msg.0) {
            Decision::Accept { events, output } => {
                self.append_and_apply(events, process_ctx).await?;
                // L-5. Delivery is one road: the output the decision stated is
                // what the caller receives, exactly once.
                Ok(output)
            }
            // L-4. Nothing was written and nothing is applied — the refusal is
            // the whole of what happened.
            Decision::Reject(rejection) => Err(AskHandlerError::Rejection(rejection)),
        }
    }
}

// Receive<TellCmd<C>>: command processing with nobody waiting (Decider path)
//
// The output is discarded because no one asked for it.  A refusal is not:
// dropping it would leave a command that was refused indistinguishable from one
// that was carried out, so it goes to the one channel that needs no receiver
// (L-5).  The same is true of a failure to persist an acceptance, which nobody
// is positioned to be told about either.
//
// Nothing here is returned as an error, because there is no one to return it to:
// `Infallible` states that in the type.

impl<A, C> Receive<TellCmd<C>> for AggregateProcess<A>
where
    A: Aggregate + Decider<C>,
    C: Send + Sync + 'static,
    <A as Decider<C>>::Rejection: std::error::Error + Send + Sync + 'static,
{
    type Response = ();
    type Error = std::convert::Infallible;

    async fn recv(
        &mut self,
        msg: TellCmd<C>,
        process_ctx: &mut ProcessContext<Self>,
    ) -> Result<Self::Response, Self::Error> {
        // The output is dropped here, before the append is awaited, rather than
        // carried along unused: nothing on this path ever reads it, and holding
        // it would make every told command's answer type `Send` for no reason.
        let events = match self.state.decide(msg.0) {
            Decision::Accept { events, .. } => events,
            Decision::Reject(rejection) => {
                tracing::warn!(
                    aggregate_id = self.aggregate_id.as_str(),
                    rejection = %rejection,
                    "a told command was refused; no caller is waiting for the refusal"
                );
                return Ok(());
            }
        };

        if let Err(e) = self.append_and_apply(events, process_ctx).await {
            tracing::error!(
                aggregate_id = self.aggregate_id.as_str(),
                error = %e,
                "a told command was accepted but its facts did not reach the stream"
            );
        }
        Ok(())
    }
}

// Receive<ExecMsg<M>>: query processing (contract::Query path)
//
// `Query::Response` and `Query::Error` carry no bounds of their own — the
// contract stays free of the machinery's vocabulary — so the bounds the runtime
// needs to carry an answer back over a channel are stated here, where the
// carrying happens.

impl<A, M> Receive<ExecMsg<M>> for AggregateProcess<A>
where
    A: Aggregate + Query<M>,
    M: Send + Sync + 'static,
    <A as Query<M>>::Response: Send + 'static,
    <A as Query<M>>::Error: std::error::Error + Send + Sync + 'static,
{
    type Response = <A as Query<M>>::Response;
    type Error = ExecHandlerError<<A as Query<M>>::Error>;

    async fn recv(
        &mut self,
        msg: ExecMsg<M>,
        _ctx: &mut ProcessContext<Self>,
    ) -> Result<Self::Response, Self::Error> {
        // Asking the state a question is a synchronous call: no identity, no
        // sequence, no await point is available to the answer (L-1).
        self.state.query(msg.0).map_err(ExecHandlerError::Domain)
    }
}

// Writing an acceptance down

/// Why the facts of an accepted decision are not in the stream.
///
/// Narrower than [`AskHandlerError`]: by the time an append is attempted the
/// decision has already been accepted, so no refusal can arise here.
#[derive(Debug, thiserror::Error)]
enum NotAppended {
    /// The genesis sequence was taken, so the aggregate already exists (C-2).
    #[error("the aggregate has already been created")]
    AlreadyCreated,
    #[error(transparent)]
    Persist(#[from] PersistError),
}

impl<R: std::error::Error + Send + Sync + 'static> From<NotAppended> for AskHandlerError<R> {
    fn from(e: NotAppended) -> Self {
        match e {
            NotAppended::AlreadyCreated => AskHandlerError::AlreadyCreated,
            NotAppended::Persist(e) => AskHandlerError::Persist(e),
        }
    }
}

impl<A: Aggregate> AggregateProcess<A> {
    /// Record the facts of an accepted decision and advance the state by them.
    ///
    /// The whole acceptance is one atomic append (L-2), so no reader can observe
    /// its second fact without its first, and the state moves only once the
    /// store has taken the facts: an activation that applied first would carry
    /// on from a state its own stream does not hold.
    ///
    /// An acceptance that produced no facts is not an empty append but no append
    /// at all (L-3) — an empty call is still one the store has to arbitrate, and
    /// nothing about a command that found its work already done needs
    /// arbitrating.
    ///
    /// This is where the conflict contract above is carried out: C-2 recognises
    /// the one conflict that is not an overtake, and C-1 stops the activation on
    /// every other one.
    async fn append_and_apply(
        &mut self,
        events: Vec<A::Event>,
        process_ctx: &mut ProcessContext<Self>,
    ) -> Result<(), NotAppended> {
        if events.is_empty() {
            return Ok(());
        }

        let mut next_sequence = self.sequence;
        let mut appending = Vec::with_capacity(events.len());
        for event in &events {
            next_sequence += 1;
            let payload = self.codec.encode(event).map_err(PersistError::Codec)?;
            appending.push(AppendingEvent {
                sequence: next_sequence,
                event_type: event.variant(),
                payload,
                occurred_at: jiff::Timestamp::now(),
            });
        }

        match self
            .store
            .append(self.aggregate_id.borrow(), appending)
            .await
        {
            Ok(_) => {}
            // C-2. Appending from sequence zero is a creation, so a conflict
            // there says the aggregate already exists.  Nothing was written, so
            // nothing is applied and the counter does not move: a later command
            // must address the genesis sequence again rather than write into a
            // stream this activation never replayed.  No decision was reached,
            // so the caller is told exactly that and handed no output (L-7).
            Err(AppendError::SequenceConflict(_)) if self.sequence == 0 => {
                return Err(NotAppended::AlreadyCreated)
            }
            // C-1. Any other conflict means the stream has been written by
            // someone else since this activation replayed it.  Stopping is the
            // whole response: reloading and retrying would only re-enter the
            // race this activation has already lost.
            Err(conflict @ AppendError::SequenceConflict(_)) => {
                // Signalled explicitly rather than left to the supervision
                // strategy, because stopping is a property of the conflict: no
                // strategy may resume or restart this activation into writing
                // the stream again.
                if let Err(e) = process_ctx.stop_self().await {
                    tracing::error!(error = %e, "stop-on-conflict could not be signalled");
                }
                return Err(PersistError::Append(conflict).into());
            }
            Err(e) => return Err(PersistError::Append(e).into()),
        }

        // Commit the sequence counter only after a successful append.
        self.sequence = next_sequence;
        for event in events {
            self.state.apply(event);
        }
        Ok(())
    }
}
