use std::pin::Pin;

use async_trait::async_trait;
use futures_core::Stream;

use crate::error::{AppendError, LoadError};
use crate::event::{AppendingEvent, LoadedEvent};
use crate::query::{AppendOutcome, LoadQuery};

pub type EventStream<'a> = Pin<Box<dyn Stream<Item = Result<LoadedEvent, LoadError>> + Send + 'a>>;

/// Append-and-load surface for events.
///
/// The `append` and `load` methods take the stream key as `&str` rather than a
/// typed identifier so the trait remains dyn-compatible (`Arc<dyn EventStore>`
/// is the framework's primary handle for plugging in a backend).  Callers
/// supply the key via `id.borrow()` (or `id.as_str()`) on their newtype —
/// `AggregateId` and `SagaId` both implement `Borrow<str>`.
#[async_trait]
pub trait EventStore: Send + Sync {
    /// Appends `events` to the stream identified by `key`.
    ///
    /// The clauses below are the optimistic-concurrency-control (OCC) contract
    /// every backend must reproduce.  They are what a third-party
    /// implementation may rely on — the incidental behaviour of any particular
    /// backend is not part of the interface.  Each clause carries a stable
    /// label (`OCC-1` … `OCC-4`) shared with the executable record in
    /// `nitinol-persistence/tests/event_store_occ.rs`, which runs against
    /// [`InMemoryEventStore`](crate::store::InMemoryEventStore) as the
    /// reference implementation.
    ///
    /// # OCC-1: `unique(stream, sequence)`
    ///
    /// Appending a `sequence` that already exists in the stream — a double
    /// append of the same `(stream, sequence)` pair — fails with
    /// [`AppendError::SequenceConflict`].  The same holds for a `sequence`
    /// repeated twice within one batch.  Detecting the duplicate is the
    /// store's obligation, not the caller's.
    ///
    /// Fixed by `occ_1_duplicate_sequence_on_same_stream_returns_sequence_conflict`
    /// and `occ_1_intra_batch_duplicate_sequence_returns_sequence_conflict`.
    ///
    /// # OCC-2: a genesis conflict means "already created"
    ///
    /// A stream's genesis sequence is `1`: producers start their counter at `0`
    /// and increment before assigning, so the first event ever appended to a
    /// stream carries `sequence = 1`.  An [`AppendError::SequenceConflict`] on
    /// the genesis sequence therefore carries a specific meaning — *this
    /// aggregate has already been created* — and the first write stays intact.
    ///
    /// Because commands are delivered at-least-once, a creation command can be
    /// redelivered.  On that path a caller may treat this conflict as a
    /// successful response rather than a failure: the requested post-state
    /// already holds.
    ///
    /// The store-side half — the conflict is returned, and the first write's
    /// content, event count, and assigned `global_sequence` are left untouched
    /// — is fixed by `occ_2_duplicate_genesis_append_preserves_first_write`.
    /// How a caller responds to a redelivered creation command is a
    /// caller-side convention and is not fixed by a test.
    ///
    /// # OCC-3: monotonicity and atomic visibility
    ///
    /// * `global_sequence` is monotonic: it increases across the store, is
    ///   assigned only on a committed write, and a conflicting append consumes
    ///   none of it.
    /// * A single `append(Vec)` becomes visible as one commit unit.  No reader
    ///   ever observes event N+1 of a batch without also observing event N;
    ///   either the whole batch is visible or none of it is.
    /// * `append(Vec)` is the AtomicWrite equivalent for a single stream, and
    ///   it is nitinol's only — and sufficient — unit of atomicity.  There is
    ///   no atomic append spanning multiple streams; cross-stream consistency
    ///   is expressed with fact events plus at-least-once process manager
    ///   (saga) delivery and idempotency.
    ///
    /// Monotonicity and all-or-nothing commit are fixed by
    /// `occ_3_conflict_does_not_advance_global_sequence_numbering`,
    /// `occ_3_batch_conflict_is_all_or_nothing_and_consumes_no_global_sequence`,
    /// and `occ_3_load_returns_events_in_ascending_global_sequence_order`.  The
    /// concurrent-reader visibility clause binds backends but is not fixed by a
    /// test: those tests pin deterministic invariants only and race no
    /// concurrent readers or writers.
    ///
    /// # OCC-4: no internal retry (tripwire semantics)
    ///
    /// A sequence conflict is a tripwire, not a condition to work around.  The
    /// store must not try to resolve it internally: no reloading the stream, no
    /// re-validating and retrying with a fresh sequence.  It returns
    /// [`AppendError::SequenceConflict`] immediately, leaving every observable
    /// part of the store — stored events and `global_sequence` numbering —
    /// exactly as it was.
    ///
    /// Re-running the backend transaction after a *transient backend* failure
    /// (lock timeout, serialization failure, dropped connection) falls outside
    /// that prohibition, subject to two conditions:
    ///
    /// * The re-run restarts the entire transaction from the beginning,
    ///   sequence validation included.  Resuming a partially executed
    ///   transaction is not permitted, because skipping revalidation could
    ///   commit a sequence a concurrent writer has taken in the meantime.
    /// * Re-runs are bounded.  Once the bound is exceeded the store stops and
    ///   returns [`AppendError::Backend`]; exhaustion is never reported as a
    ///   sequence conflict or as success.
    ///
    /// Immediate reporting with unchanged observable state is fixed by
    /// `occ_4_conflicting_append_leaves_observable_state_unchanged`.  The
    /// conditions on backend transaction re-runs bind backends but are not
    /// fixed by a test.
    async fn append(
        &self,
        key: &str,
        events: Vec<AppendingEvent>,
    ) -> Result<AppendOutcome, AppendError>;

    async fn load(&self, query: LoadQuery) -> Result<EventStream<'_>, LoadError>;
}
