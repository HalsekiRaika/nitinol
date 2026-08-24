//! Operator-facing **pull** API over one saga's persisted dead letters.
//!
//! The push path ([`SagaProps::with_dead_letter_subscriber`]) delivers dead
//! letters as they land — an observability signal.  [`DeadLetterQueue`] is the
//! recovery counterpart: it lists what is still outstanding and records what an
//! operator did about it.
//!
//! Everything runs over the [`EventStore`] alone, so the queue serves a saga
//! that is not resident — which is exactly the situation an operator triaging a
//! stopped saga is in.
//!
//! Reprocessing is deliberately absent.  A [`DeadLetterEntry`] carries the
//! recovery material (the failure's raw payload plus its
//! [`SourceContext`](crate::SourceContext) coordinates); deciding what to do
//! with it is the downstream application's, not the framework's.
//!
//! [`SagaProps::with_dead_letter_subscriber`]: crate::SagaProps::with_dead_letter_subscriber

use std::collections::HashSet;
use std::sync::Arc;

use futures_util::TryStreamExt;
use nitinol_eventsource::{appending_system_event, SystemEvent};
use nitinol_persistence::error::{AppendError, LoadError};
use nitinol_persistence::store::EventStore;
use nitinol_persistence::{LoadQuery, LoadedEvent};

use crate::id::SagaId;

use super::disposition::{DeadLetterDispositionEvent, Disposition, DEAD_LETTER_DISPOSITION_MARKER};
use super::event::{is_dead_letter_event_type, DeadLetterEvent, DEAD_LETTER_MARKER};

/// Filter and paging for [`DeadLetterQueue::list`].
///
/// The default query returns every unsettled dead letter on the stream.
#[derive(Clone, Debug, Default)]
pub struct DeadLetterQuery {
    /// Inclusive lower bound on the dead letter's own stream sequence.  A dead
    /// letter sitting exactly on the bound is returned.
    pub from_sequence: Option<u64>,
    /// Upper bound on the number of entries returned.
    ///
    /// Applied **after** settled dead letters are excluded, so it caps entries
    /// rather than the underlying scan: a limit pushed down to the store would
    /// truncate the scan first and return fewer entries than are available.
    pub limit: Option<usize>,
}

/// One dead letter that no disposition marker has settled.
#[derive(Clone, Debug)]
pub struct DeadLetterEntry {
    /// Stream sequence the dead letter occupies on the saga's own stream — the
    /// handle [`DeadLetterQueue::mark_processed`] and [`DeadLetterQueue::evict`]
    /// take.
    pub sequence: u64,
    /// The persisted failure record, including the recovery material a
    /// downstream application would reprocess from.
    pub event: DeadLetterEvent,
}

/// Error produced by a [`DeadLetterQueue`] operation.
#[derive(Debug, thiserror::Error)]
pub enum DeadLetterQueueError {
    /// The named sequence holds no dead letter on this saga's stream — it holds
    /// some other record, or nothing at all.
    ///
    /// A disposition marker names a stream sequence, so one written against a
    /// sequence that is not a dead letter could never be resolved back to what
    /// it settled.
    #[error("saga stream {saga_id:?} carries no dead letter at sequence {sequence}")]
    NotADeadLetter { saga_id: SagaId, sequence: u64 },
    #[error("reading the saga stream failed: {0}")]
    Load(#[from] LoadError),
    #[error("appending the disposition marker failed: {0}")]
    Append(#[from] AppendError),
    /// A record on the saga's stream could not be decoded.
    ///
    /// Reported rather than skipped: a dead letter the queue cannot read is one
    /// the operator would never be shown, and a disposition marker it cannot
    /// read would resurface an already-settled dead letter.
    #[error("decoding a persisted dead letter record failed: {0}")]
    Decode(Box<dyn std::error::Error + Send + Sync>),
}

/// Pull API over the dead letters on one saga's own EventStore stream.
///
/// Scoped to a single [`SagaId`]: one physical store hosts every saga's stream
/// side by side, and a queue only ever reads and writes its own.
///
/// # Concurrency with a resident saga
///
/// This queue is meant for a saga instance that is **not resident** — the
/// operator-triage situation described in the module doc above. A resident
/// `SagaProcess` owns its stream's sequence in memory and keeps appending to
/// it, so calling [`mark_processed`](Self::mark_processed) or
/// [`evict`](Self::evict) while that instance is running races those
/// in-process appends for the same stream:
/// - If this queue's append loses the race, the caller sees
///   [`DeadLetterQueueError::Append`] and no marker is written.
/// - If the saga process's append loses instead, its staged retry resubmits
///   the same sequence and collides again, the `PersistFailed` dead letter it
///   then tries to enqueue collides for the same reason, and the process
///   stops on that spurious `persist_failed`. A supervised restart replays
///   the stream, resynchronizes the process's sequence, and recovers.
pub struct DeadLetterQueue {
    store: Arc<dyn EventStore>,
    saga_id: SagaId,
}

impl DeadLetterQueue {
    /// Open the queue for `saga_id` over `store` — the same store the saga
    /// persists its own stream to.
    pub fn new(store: Arc<dyn EventStore>, saga_id: SagaId) -> Self {
        Self { store, saga_id }
    }

    /// The dead letters this saga has enqueued and nobody has settled,
    /// in ascending stream-sequence order.
    pub async fn list(
        &self,
        query: DeadLetterQuery,
    ) -> Result<Vec<DeadLetterEntry>, DeadLetterQueueError> {
        let settled = self.settled_sequences().await?;

        let dead_letters = self
            .load(LoadQuery {
                stream_key: Some(self.saga_id.as_str().to_owned()),
                event_type_prefix: Some(DEAD_LETTER_MARKER.to_path()),
                from_stream_sequence: query.from_sequence,
                ..Default::default()
            })
            .await?;

        let mut entries = Vec::new();
        for loaded in dead_letters {
            if settled.contains(&loaded.sequence) {
                continue;
            }
            entries.push(DeadLetterEntry {
                sequence: loaded.sequence,
                event: DeadLetterEvent::decode(&loaded.payload)
                    .map_err(|e| DeadLetterQueueError::Decode(Box::new(e)))?,
            });
        }

        // `from_sequence` pages over the dead letter's own stream sequence, so
        // the order `limit` cuts in has to be that same sequence — not the
        // store's global ordering, which the load contract guarantees instead.
        entries.sort_unstable_by_key(|entry| entry.sequence);
        if let Some(limit) = query.limit {
            entries.truncate(limit);
        }
        Ok(entries)
    }

    /// Record that the dead letter at `sequence` was dealt with downstream.
    ///
    /// Appends a `processed` disposition marker; the dead letter leaves
    /// [`list`](Self::list) and its original record stays in the store.
    pub async fn mark_processed(&self, sequence: u64) -> Result<(), DeadLetterQueueError> {
        self.append_disposition(sequence, Disposition::Processed)
            .await
    }

    /// Retire the dead letter at `sequence` without dealing with it.
    ///
    /// This is a **logical** delete: an `evicted` disposition marker is
    /// appended, the dead letter leaves [`list`](Self::list), and the original
    /// record stays in the store.  An event log has no other kind of delete.
    pub async fn evict(&self, sequence: u64) -> Result<(), DeadLetterQueueError> {
        self.append_disposition(sequence, Disposition::Evicted)
            .await
    }

    /// Stream sequences of every dead letter a disposition marker has settled.
    ///
    /// Read unfiltered and unpaged: a marker anywhere on the stream settles its
    /// dead letter, and the markers sit at the stream's tail rather than beside
    /// what they settle.  Narrowing this scan to the caller's window would let
    /// an already-settled dead letter resurface.
    async fn settled_sequences(&self) -> Result<HashSet<u64>, DeadLetterQueueError> {
        let markers = self
            .load(LoadQuery {
                stream_key: Some(self.saga_id.as_str().to_owned()),
                event_type_prefix: Some(DEAD_LETTER_DISPOSITION_MARKER.to_path()),
                ..Default::default()
            })
            .await?;

        markers
            .iter()
            .map(|loaded| {
                DeadLetterDispositionEvent::decode(&loaded.payload)
                    .map(|marker| marker.dead_letter_sequence)
                    .map_err(|e| DeadLetterQueueError::Decode(Box::new(e)))
            })
            .collect()
    }

    async fn append_disposition(
        &self,
        sequence: u64,
        disposition: Disposition,
    ) -> Result<(), DeadLetterQueueError> {
        // One pass over the stream answers both questions the append needs: is
        // `sequence` really a dead letter, and where does the stream end.  The
        // store offers no way to read the tail without scanning, and the guard
        // has to see the whole stream anyway.
        let stream = self.load(LoadQuery::by_stream(&self.saga_id)).await?;
        let mut settles_a_dead_letter = false;
        let mut tail = 0;
        for loaded in &stream {
            tail = tail.max(loaded.sequence);
            if loaded.sequence == sequence && is_dead_letter_event_type(loaded.event_type) {
                settles_a_dead_letter = true;
            }
        }
        if !settles_a_dead_letter {
            return Err(DeadLetterQueueError::NotADeadLetter {
                saga_id: self.saga_id.clone(),
                sequence,
            });
        }

        let marker = DeadLetterDispositionEvent {
            dead_letter_sequence: sequence,
            disposition,
        };
        // A resident saga writes to this same stream, so the tail read above can
        // go stale.  The store's `unique(stream, sequence)` rule turns that race
        // into a `SequenceConflict` (see the doc on `DeadLetterQueue` above): either
        // this append loses and the caller sees it as `DeadLetterQueueError::Append`
        // with no marker written, or the saga process's own append loses instead and
        // the process stops on a spurious `persist_failed` until a restart resyncs it.
        self.store
            .append(
                self.saga_id.as_str(),
                vec![appending_system_event(
                    tail + 1,
                    &marker,
                    jiff::Timestamp::now(),
                )],
            )
            .await?;
        Ok(())
    }

    async fn load(&self, query: LoadQuery) -> Result<Vec<LoadedEvent>, DeadLetterQueueError> {
        Ok(self.store.load(query).await?.try_collect().await?)
    }
}
