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
    /// backend is not part of the interface — and they change only as the
    /// crate's version does.  Each clause carries a stable label (`OCC-1` …
    /// `OCC-5`).
    ///
    /// Two executable records share those labels.
    /// `nitinol-persistence/tests/event_store_occ.rs` runs the deterministic
    /// clauses against [`InMemoryEventStore`](crate::store::InMemoryEventStore)
    /// as the reference implementation.  `nitinol-store-conformance` is the
    /// suite any backend is put through, reference implementation included; it
    /// is where a clause that needs a reader and a writer at the same time is
    /// fixed, and a backend that has not been through it has not been shown to
    /// keep this contract.
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
    /// and `occ_3_load_returns_events_in_ascending_global_sequence_order`,
    /// which race no concurrent readers or writers, and by the deterministic
    /// half of `nitinol-store-conformance` for any other backend.  *When* a
    /// commit may become visible to a reader that is already reading is OCC-5's
    /// subject, and is fixed there.
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
    ///
    /// # OCC-5: a reader's checkpoint is never undercut
    ///
    /// Once a reader has observed `global_sequence = N`, no event carrying a
    /// `global_sequence` at or below `N` becomes newly visible afterwards.
    ///
    /// *Newly visible* means visible for the first time, in the same store and
    /// the same global ordering.  Reading events a reader has already passed is
    /// not covered: replaying from an older checkpoint, or widening a query's
    /// filter, makes nothing visible that was not visible before.
    ///
    /// This is the clause that makes a checkpoint-following reader safe.  Such
    /// a reader records the highest `global_sequence` it has observed and asks
    /// only for what lies beyond it — the semantics
    /// `nitinol_eventsource`'s global cursor is built on — so an event
    /// appearing below that mark is read by nobody.  Nothing detects it: the
    /// event is stored, it is intact, every append returned success, and no
    /// error is raised at the writer, at the reader, or in between.
    ///
    /// A backend breaks this clause by assigning `global_sequence` outside the
    /// transaction that commits the event — a plain auto-increment column taken
    /// before the commit is the usual way — because two concurrent writers then
    /// commit in an order other than the one they were numbered in.  Under
    /// appends from several nodes at once that is the normal case, not an edge
    /// one.  Numbering and becoming visible have to happen in the one order.
    ///
    /// Fixed by the concurrency half of `nitinol-store-conformance`, which runs
    /// concurrent writers against a reader following the global order by
    /// checkpoint and requires that reader to have observed every committed
    /// event exactly once.
    async fn append(
        &self,
        key: &str,
        events: Vec<AppendingEvent>,
    ) -> Result<AppendOutcome, AppendError>;

    /// Serves the stored events a [`LoadQuery`] selects.
    ///
    /// # LOAD-1: a query's conditions are a conjunction
    ///
    /// Every condition a `LoadQuery` names applies to every event served: what
    /// comes back satisfies all of them, in ascending `global_sequence` order,
    /// and nothing else does. A condition left absent selects nothing out.
    ///
    /// In particular `event_type_prefix` and `from_global_sequence` named
    /// together serve the events within that prefix **and** at or above that
    /// position. Neither one widens the other: an event within the prefix below
    /// the position is left out, and so is an event above the position outside
    /// the prefix.
    ///
    /// `from_global_sequence` includes the position it names, so a consumer
    /// asks for what it has not seen by naming the position after the one it
    /// finished with.
    ///
    /// This is the reading every checkpoint-following consumer of one event
    /// type rests on, and both halves matter to it. An event of another type
    /// coming back would be handed to a consumer that cannot read it; an event
    /// at or below the position would be handed over a second time. Ascending
    /// order is what lets such a consumer take the last event it finished with
    /// as its progress.
    ///
    /// A prefix matches on path boundaries: a stored path is within the prefix
    /// when it is the prefix itself, or when the prefix is followed by a
    /// separator. A canonical path — a family and a type name, with nothing
    /// after it — has no descendant of its own, so naming one selects that
    /// declaration's events alone.
    ///
    /// Fixed by `nitinol-store-conformance` for any backend, and by
    /// `nitinol-persistence/tests/event_store_load_query_conjunction.rs`
    /// against the reference implementation.
    async fn load(&self, query: LoadQuery) -> Result<EventStream<'_>, LoadError>;
}

/// A journal reached through an `Arc` is that journal.
///
/// What this is for is naming a store that is not a cloneable value of its own
/// — a trait object, or a backend handle a deployment shares — as a slot of an
/// assembly: the `Arc` is the cloneable handle, and the clauses above are
/// whatever the store behind it keeps.
#[async_trait]
impl<T: EventStore + ?Sized> EventStore for std::sync::Arc<T> {
    async fn append(
        &self,
        key: &str,
        events: Vec<AppendingEvent>,
    ) -> Result<AppendOutcome, AppendError> {
        T::append(self, key, events).await
    }

    async fn load(&self, query: LoadQuery) -> Result<EventStream<'_>, LoadError> {
        T::load(self, query).await
    }
}
