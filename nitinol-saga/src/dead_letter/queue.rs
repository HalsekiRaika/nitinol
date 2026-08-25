//! Operator-facing **pull** API over one saga's persisted dead letters.
//!
//! The push path ([`SagaProps::with_dead_letter_subscriber`]) delivers dead
//! letters as they land — an observability signal.  [`DeadLetterQueue`] is the
//! recovery counterpart: it lists what is still outstanding and records what an
//! operator did about it.
//!
//! Listing runs over the [`EventStore`] alone.  Settling does not: the marker
//! is appended to the saga's *own* stream, and while an instance is resident it
//! is that instance — not this queue — that owns the stream's next sequence.  A
//! queue obtained from [`SagaManagerProxy::dead_letter_queue`] therefore hands
//! the write to the manager, the single arbiter of every stream in its fan-out;
//! see [`DeadLetterQueue`] for the case where there is no arbiter.
//!
//! Reprocessing is deliberately absent.  A [`DeadLetterEntry`] carries the
//! recovery material (the failure's raw payload plus its
//! [`SourceContext`](crate::SourceContext) coordinates); deciding what to do
//! with it is the downstream application's, not the framework's.
//!
//! [`SagaProps::with_dead_letter_subscriber`]: crate::SagaProps::with_dead_letter_subscriber
//! [`SagaManagerProxy::dead_letter_queue`]: crate::SagaManagerProxy::dead_letter_queue

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
use super::settle::{DispositionArbiter, SettleError};

/// A settle that did not happen, told the way the operator asked the question.
///
/// The arbiter's own distinctions survive: a store refusal stays an append
/// failure, a stream that could not be read stays a load failure.  Only the
/// arbiter being out of reach is new, because only the arbitrated path has one.
impl From<SettleError> for DeadLetterQueueError {
    fn from(error: SettleError) -> Self {
        match error {
            SettleError::Append(e) => Self::Append(e),
            SettleError::Load(e) => Self::Load(e),
            SettleError::Unreachable(detail) => Self::ArbiterUnreachable(detail),
        }
    }
}

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
    /// The saga manager this queue routes its writes through did not answer, so
    /// the disposition was not recorded.
    ///
    /// Nothing was written and the dead letter is still outstanding: the
    /// operation can simply be run again once the manager — or the instance it
    /// routed to — is available.
    #[error("recording the disposition through the saga manager failed: {0}")]
    ArbiterUnreachable(String),
}

/// Pull API over the dead letters on one saga's own EventStore stream.
///
/// Scoped to a single [`SagaId`]: one physical store hosts every saga's stream
/// side by side, and a queue only ever reads and writes its own.
///
/// # Who writes the marker
///
/// [`list`](Self::list) reads the store. [`mark_processed`](Self::mark_processed)
/// and [`evict`](Self::evict) write to the saga's own stream, which has exactly
/// one writer at any moment — so they do not append themselves. A queue built by
/// [`SagaManagerProxy::dead_letter_queue`] hands the marker to that manager,
/// which appends through the resident instance's mailbox when the saga is
/// resident and writes it itself when the saga is not. Either way the operator
/// waits for the outcome and no append ever contends for the stream's next
/// sequence.
///
/// # Standalone use, without an arbiter
///
/// A queue built with [`new`](Self::new) has no arbiter: it holds a store and a
/// saga id and nothing that knows whether that saga is running. It appends the
/// marker after the stream tail it just read, which is correct **only while
/// nothing else is writing that stream** — the caller is the one guaranteeing
/// that. This covers offline triage, and also a saga spawned standalone through
/// [`SagaProps`](crate::SagaProps), which has no manager to arbitrate for it.
///
/// If that guarantee does not hold, the tail read goes stale between the read
/// and the append and the store's `unique(stream, sequence)` rule turns the
/// overlap into a conflict — either this append fails, or the running saga's
/// does and it stops on a `persist_failed` that describes nothing but the
/// collision. Route through the manager whenever there is one.
///
/// [`SagaManagerProxy::dead_letter_queue`]: crate::SagaManagerProxy::dead_letter_queue
pub struct DeadLetterQueue {
    store: Arc<dyn EventStore>,
    saga_id: SagaId,
    /// The single writer of this saga's stream, when one exists.
    ///
    /// `None` is the standalone case documented above: no arbiter exists, and
    /// the caller carries the guarantee an arbiter would have enforced.
    arbiter: Option<Arc<dyn DispositionArbiter>>,
}

impl DeadLetterQueue {
    /// Open the queue for `saga_id` over `store` — the same store the saga
    /// persists its own stream to.
    ///
    /// The resulting queue has no arbiter; see the type doc for what the caller
    /// is guaranteeing by using it.
    pub fn new(store: Arc<dyn EventStore>, saga_id: SagaId) -> Self {
        Self {
            store,
            saga_id,
            arbiter: None,
        }
    }

    /// Open the queue for `saga_id`, routing its disposition writes through
    /// `arbiter` — the single writer of that saga's stream.
    ///
    /// `store` is the arbiter's own store, so what [`list`](Self::list) reads
    /// and what the arbiter writes cannot drift apart.
    pub(crate) fn arbitrated(
        store: Arc<dyn EventStore>,
        saga_id: SagaId,
        arbiter: Arc<dyn DispositionArbiter>,
    ) -> Self {
        Self {
            store,
            saga_id,
            arbiter: Some(arbiter),
        }
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
        // The guard has to see the whole stream to answer "is `sequence` really
        // a dead letter", and the store offers no way to read the tail without
        // scanning either — so the arbiterless path's tail comes out of the
        // same pass rather than a second one.
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
        // What to write was decided here, because deciding it needs to know what
        // a dead letter is.  Where it goes is the arbiter's decision, because
        // that needs to know who currently owns the stream's next sequence.
        match &self.arbiter {
            Some(arbiter) => arbiter.settle(&self.saga_id, marker).await?,
            None => {
                // No arbiter: the caller has guaranteed nothing else is writing
                // this stream, so the tail read above is still the tail.
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
            }
        }
        Ok(())
    }

    async fn load(&self, query: LoadQuery) -> Result<Vec<LoadedEvent>, DeadLetterQueueError> {
        Ok(self.store.load(query).await?.try_collect().await?)
    }
}
