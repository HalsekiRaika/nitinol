//! Replaying a saga's own stream: a fold and nothing else.
//!
//! Replay restores two things — the saga's state and how far each of its
//! sources has been consumed — and reports the schedules still pending so
//! `on_start` can re-arm them.  It decides nothing and writes nothing: there is
//! no re-dispatch to resume, no fact to synthesise, and no record to file about
//! what it read.
//!
//! A row it cannot read stops it.  Four kinds of row make up a saga stream — a
//! domain event, a schedule fact, a consumed fact, a lifecycle fact — and
//! anything else inside the framework's own namespace is a corrupted stream or
//! a version mismatch, as is a row of a recognised declaration whose payload
//! will not decode.  Both refuse the load rather than being read past, so a
//! stream the running model cannot account for never reaches
//! [`Saga::handle`](crate::Saga::handle).

use futures_util::StreamExt;
use nitinol_persistence::store::Journal;
use nitinol_persistence::{LoadQuery, LoadedEvent};

use crate::consumed::ConsumedPositions;
use crate::id::SagaId;
use crate::journal::{ActiveSchedule, JournalState};
use crate::persisted::SagaPersisted;
use crate::saga::Saga;

/// Outcome of replaying the saga's own event stream on `on_start`.
pub(crate) struct ReplayOutcome {
    /// `true` when the stream carries a durable `Ended` lifecycle fact — the
    /// saga terminated in a previous incarnation and must not be revived.
    pub(crate) ended: bool,
    /// Schedules still pending after replay, to be re-registered with the
    /// scheduler.  Empty when the saga has ended.
    pub(crate) active_schedules: Vec<ActiveSchedule>,
    /// How far each upstream source was consumed by the incarnations before
    /// this one.  Reported for an ended saga as well: what a previous
    /// incarnation consumed is a fact about the stream, not a decision about
    /// what to do next.
    pub(crate) consumed: ConsumedPositions,
}

/// Fold the saga's own stream into [`ReplayOutcome`], applying its domain
/// events to `state` in stream order and advancing `sequence` to the tail.
///
/// Returns `Err(())` when the stream could not be read to its end — the store
/// refused the load, the iteration failed, or a row belongs to no declaration
/// this version can read.  The caller then stops the process without
/// subscribing: a saga whose own history cannot be read must not act on it.
pub(crate) async fn replay<S: Saga>(
    saga_id: &SagaId,
    state: &mut S,
    store: &Journal,
    sequence: &mut u64,
) -> Result<ReplayOutcome, ()> {
    let journal = load_journal(saga_id, state, store, *sequence).await?;
    *sequence = journal.sequence;
    if journal.ended {
        // A terminated saga re-arms nothing: it will never reach `handle`
        // again, so a deadline it set has no arm left to fire into.
        return Ok(ReplayOutcome {
            ended: true,
            active_schedules: Vec::new(),
            consumed: journal.consumed,
        });
    }
    Ok(ReplayOutcome {
        ended: false,
        active_schedules: journal.active_schedules.into_values().collect(),
        consumed: journal.consumed,
    })
}

/// Load the saga's own stream from `from_sequence + 1` and fold it into a
/// [`JournalState`], applying the domain events it hands back to `state` in
/// stream order.
async fn load_journal<S: Saga>(
    saga_id: &SagaId,
    state: &mut S,
    store: &Journal,
    from_sequence: u64,
) -> Result<JournalState, ()> {
    let query = LoadQuery {
        stream_key: Some(saga_id.as_str().to_owned()),
        from_stream_sequence: Some(from_sequence + 1),
        ..Default::default()
    };
    let stream = match store.load(query).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = ?e, "saga event store load failed during replay");
            return Err(());
        }
    };

    futures_util::pin_mut!(stream);
    let mut journal = JournalState::new(from_sequence);
    while let Some(item) = stream.next().await {
        let loaded = match item {
            Ok(ev) => ev,
            Err(e) => {
                tracing::error!(error = ?e, "saga event store stream error during replay");
                return Err(());
            }
        };
        fold_loaded::<S>(loaded, state, &mut journal)?;
    }
    Ok(journal)
}

/// Classify one loaded record and fold it, applying a domain event to the
/// saga's state.
///
/// A row that cannot be read refuses the whole load.  Reading past it would
/// either fold a state missing one of the saga's own decisions, or claim a
/// position no incarnation ever reached — and a row of a withdrawn declaration
/// may well decode cleanly as a domain event, so "report it and carry on"
/// would quietly turn a bygone mechanism's bookkeeping into domain history.
fn fold_loaded<S: Saga>(
    loaded: LoadedEvent,
    state: &mut S,
    journal: &mut JournalState,
) -> Result<(), ()> {
    journal.observe_sequence(loaded.sequence);
    match SagaPersisted::classify(loaded.event_type, &loaded.payload) {
        Ok(persisted) => {
            if let Some(event) = journal.fold(persisted) {
                state.apply(event);
            }
            Ok(())
        }
        Err(e) => {
            tracing::error!(
                error = %e,
                event_type = %loaded.event_type,
                sequence = loaded.sequence,
                "saga replay refused a row of its own stream; the load fails rather \
                 than reading past it",
            );
            Err(())
        }
    }
}
