//! What a caller brings to the kit: a store to put under test, and a runtime to
//! run the concurrent clauses on.
//!
//! One of these per store contract the kit states. [`ReferenceStore`],
//! [`ReferenceSnapshotStore`] and [`ReferenceOffsetStore`] bring up the
//! in-memory implementation the `OCC`, `SNAP` and `OFS` clauses are
//! respectively written against, and [`TokioExecutor`] is the runtime the
//! concurrent clauses are lent. Together they are the whole of the wiring a
//! provider writes.
//!
//! [`FaultyStore`], [`FaultySnapshotStore`] and [`FaultyOffsetStore`] are
//! the kit's own self-check: each brings up a store that breaks exactly the one
//! clause its fault names, so the clauses can be shown to catch a backend that
//! breaks the contract instead of merely passing against one that keeps it.
//!
//! [`ReferenceProvider`] is the same wiring one level up: a whole store, from
//! which a supplier is assembled, rather than a single slot — brought up for
//! the `PRV` clauses. [`FaultyProvider`],
//! [`ProviderWithCloneThatSplitsTheWorld`] and
//! [`ProviderWithCollidingOffsetSlots`] are its self-check, each breaking
//! exactly the one clause it names.
//!
//! The doubles live in this crate rather than in the kit's `src/` for two
//! reasons: they are not part of what the kit publishes, and a store defined
//! outside the kit's own crate is the direct evidence that a provider's store
//! can be handed to the public API.
//!
//! Each test binary compiles this module in full but uses only the subset it
//! needs, so per-binary dead code is expected.

#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};

use async_trait::async_trait;
use futures_core::future::BoxFuture;
use futures_util::stream;
use nitinol_persistence::error::{AppendError, LoadError, OffsetError, SnapshotError};
use nitinol_persistence::provider::{CustomStore, Store, Unprovided};
use nitinol_persistence::store::{
    Album, EventStore, EventStream, InMemoryEventStore, InMemoryOffsetStore, InMemorySnapshotStore,
    InMemoryStore, Journal, OffsetScope, OffsetStore, SnapshotStore,
};
use nitinol_persistence::{
    is_within_reserved_namespace, AggregateId, AppendOutcome, AppendingEvent, LoadQuery,
    LoadedEvent, PersistedSnapshot,
};
use nitinol_store_conformance::offset::OffsetStoreUnderTest;
use nitinol_store_conformance::provider::ProviderUnderTest;
use nitinol_store_conformance::snapshot::SnapshotStoreUnderTest;
use nitinol_store_conformance::{Executor, StoreUnderTest};

/// The reference implementation the OCC clauses are written against.
pub struct ReferenceStore;

impl StoreUnderTest for ReferenceStore {
    fn bring_up(&self) -> BoxFuture<'_, Journal> {
        Box::pin(async { Journal::from(InMemoryEventStore::default()) })
    }
}

/// The caller's runtime, lent to the kit for the clauses that need writers to
/// make progress independently of the reader.
///
/// `spawn` awaits the join handle so a writer that dies reaches the clause
/// rather than disappearing into a dropped handle. `yield_now` is the concession
/// point the reader's polling loop needs: a store whose `load` never awaits
/// would otherwise starve the writers.
pub struct TokioExecutor;

impl Executor for TokioExecutor {
    fn spawn(&self, task: BoxFuture<'static, ()>) -> BoxFuture<'static, ()> {
        let handle = tokio::spawn(task);
        Box::pin(async move {
            handle
                .await
                .expect("a task the suite spawned must run to completion");
        })
    }

    fn yield_now(&self) -> BoxFuture<'static, ()> {
        Box::pin(tokio::task::yield_now())
    }
}

/// The way a [`SkewedEventStore`] lets the order it numbers events in come apart
/// from the order it makes them visible in.
#[derive(Debug, Clone, Copy)]
pub enum Fault {
    /// The first append takes its `global_sequence` and then withholds the
    /// events until a reader has already been served a higher one.
    ///
    /// This is what a counter handed out before the commit it belongs to — a
    /// plain auto-increment column, assigned outside the transaction — does
    /// under two concurrent writers, and it is the OCC-5 violation: a
    /// offset reader that has passed `N` never asks for anything at or
    /// below `N` again, so the late event is lost without an error anywhere.
    DelayFirstAppendVisibility,
    /// A batch of two or more events is served one event short to the first
    /// load that would return it, which breaks OCC-3's commit unit.
    WithholdLastEventOfMultiEventBatch,
}

/// A store under test that brings up a backend breaking exactly the one clause
/// `fault` names.
pub struct FaultyStore {
    fault: Fault,
}

impl FaultyStore {
    pub fn injecting(fault: Fault) -> Self {
        Self { fault }
    }
}

impl StoreUnderTest for FaultyStore {
    fn bring_up(&self) -> BoxFuture<'_, Journal> {
        let fault = self.fault;
        Box::pin(async move { Journal::from(SkewedEventStore::injecting(fault)) })
    }
}

/// How many yields a withheld append waits for a reader to pass it before
/// publishing anyway.
///
/// The bound is a fail-safe, not the mechanism: a clause that never reads must
/// not hang the suite, while the release a violation needs is the reader's own
/// progress and not a count or a clock.
const YIELDS_AWAITING_A_READER: usize = 10_000;

struct SkewedState {
    visible: Vec<LoadedEvent>,
    /// Numbered, acknowledged to nobody yet, and not in `visible`.
    withheld: Vec<LoadedEvent>,
    /// `global_sequence`es to drop from the next load that would return them.
    hidden_once: HashSet<u64>,
    next_global_sequence: u64,
    highest_global_sequence_a_load_returned: u64,
}

/// A store that keeps the OCC contract except for the clause its fault names.
///
/// It owns its numbering instead of decorating [`InMemoryEventStore`]: pulling
/// the order events are numbered in apart from the order they become visible in
/// takes hold of the counter itself.
/// The state is behind an [`Arc`] because this store is a *handle*: a clone
/// reaching a region of its own would be the `PRV-1` fault rather than the
/// one clause the fault names, and the suite distributes the handle by cloning
/// it.
#[derive(Clone)]
pub struct SkewedEventStore {
    fault: Fault,
    state: Arc<Mutex<SkewedState>>,
}

/// What one `append` left behind, and whether a reader has to pass it before it
/// may be published.
struct Staged {
    outcome: AppendOutcome,
    publish_once_a_reader_has_passed: Option<u64>,
}

impl SkewedEventStore {
    fn injecting(fault: Fault) -> Self {
        Self {
            fault,
            state: Arc::new(Mutex::new(SkewedState {
                visible: Vec::new(),
                withheld: Vec::new(),
                hidden_once: HashSet::new(),
                // global_sequence starts at 1, as it does in the reference
                // implementation.
                next_global_sequence: 1,
                highest_global_sequence_a_load_returned: 0,
            })),
        }
    }

    fn lock(&self) -> MutexGuard<'_, SkewedState> {
        self.state
            .lock()
            .expect("the skewed store's state lock was poisoned by a panicking holder")
    }

    fn stage(&self, key: &str, events: Vec<AppendingEvent>) -> Result<Staged, AppendError> {
        let mut state = self.lock();

        // Conflict detection counts withheld events too: withholding delays
        // when a write becomes readable, and must not turn a taken sequence
        // back into a free one.
        let mut taken: HashSet<u64> = state
            .visible
            .iter()
            .chain(state.withheld.iter())
            .filter(|event| event.stream_key == key)
            .map(|event| event.sequence)
            .collect();
        for event in &events {
            if !taken.insert(event.sequence) {
                return Err(AppendError::SequenceConflict(key.to_owned()));
            }
        }

        let withhold = matches!(self.fault, Fault::DelayFirstAppendVisibility)
            && state.next_global_sequence == 1;

        let mut assigned_sequences = Vec::with_capacity(events.len());
        let mut staged = Vec::with_capacity(events.len());
        for event in events {
            let global_sequence = state.next_global_sequence;
            state.next_global_sequence += 1;
            assigned_sequences.push(global_sequence);
            staged.push(LoadedEvent {
                stream_key: key.to_owned(),
                sequence: event.sequence,
                global_sequence,
                event_type: event.event_type,
                payload: event.payload,
                occurred_at: event.occurred_at,
            });
        }

        if matches!(self.fault, Fault::WithholdLastEventOfMultiEventBatch) && staged.len() > 1 {
            let last = staged
                .last()
                .expect("a batch of more than one event has a last event")
                .global_sequence;
            state.hidden_once.insert(last);
        }

        let publish_once_a_reader_has_passed = if withhold {
            staged.last().map(|event| event.global_sequence)
        } else {
            None
        };
        if publish_once_a_reader_has_passed.is_some() {
            state.withheld.extend(staged);
        } else {
            state.visible.extend(staged);
        }

        // The stream's highest sequence, which is what the reference
        // implementation reports for an empty batch and what it reports for
        // every ascending one.  The clauses judge `assigned_sequences`.
        let stream_version = state
            .visible
            .iter()
            .chain(state.withheld.iter())
            .filter(|event| event.stream_key == key)
            .map(|event| event.sequence)
            .max()
            .unwrap_or(0);

        Ok(Staged {
            outcome: AppendOutcome {
                assigned_sequences,
                stream_version,
            },
            publish_once_a_reader_has_passed,
        })
    }

    async fn wait_until_a_reader_has_passed(&self, global_sequence: u64) {
        for _ in 0..YIELDS_AWAITING_A_READER {
            let a_reader_has_passed =
                self.lock().highest_global_sequence_a_load_returned > global_sequence;
            if a_reader_has_passed {
                return;
            }
            tokio::task::yield_now().await;
        }
    }

    fn publish_withheld(&self) {
        let mut state = self.lock();
        let withheld = std::mem::take(&mut state.withheld);
        state.visible.extend(withheld);
    }
}

#[async_trait]
impl EventStore for SkewedEventStore {
    async fn append(
        &self,
        key: &str,
        events: Vec<AppendingEvent>,
    ) -> Result<AppendOutcome, AppendError> {
        let staged = self.stage(key, events)?;

        if let Some(global_sequence) = staged.publish_once_a_reader_has_passed {
            self.wait_until_a_reader_has_passed(global_sequence).await;
            self.publish_withheld();
        }

        Ok(staged.outcome)
    }

    async fn load(&self, query: LoadQuery) -> Result<EventStream<'_>, LoadError> {
        let mut state = self.lock();

        let mut matching: Vec<LoadedEvent> = state
            .visible
            .iter()
            .filter(|event| matches_query(event, &query))
            .cloned()
            .collect();
        matching.retain(|event| !state.hidden_once.remove(&event.global_sequence));
        matching.sort_unstable_by_key(|event| event.global_sequence);
        if let Some(limit) = query.limit {
            matching.truncate(limit);
        }

        // What a reader has been served is what the delayed append waits on, so
        // it is recorded here and nowhere else.
        if let Some(highest) = matching.last().map(|event| event.global_sequence) {
            state.highest_global_sequence_a_load_returned =
                state.highest_global_sequence_a_load_returned.max(highest);
        }
        drop(state);

        Ok(Box::pin(stream::iter(matching.into_iter().map(Ok))))
    }
}

/// The filter the reference implementation applies, so the only way this store
/// differs from a conforming one is the fault it injects.
fn matches_query(event: &LoadedEvent, query: &LoadQuery) -> bool {
    if let Some(ref key) = query.stream_key {
        if &event.stream_key != key {
            return false;
        }
    }
    if let Some(ref prefix) = query.event_type_prefix {
        if !event.event_type.to_path().is_within(prefix) {
            return false;
        }
    }
    if let Some(from_global) = query.from_global_sequence {
        if event.global_sequence < from_global {
            return false;
        }
    }
    if let Some(from_stream_sequence) = query.from_stream_sequence {
        if event.sequence < from_stream_sequence {
            return false;
        }
    }
    true
}

/// The reference implementation the `SNAP` clauses are written against.
pub struct ReferenceSnapshotStore;

impl SnapshotStoreUnderTest for ReferenceSnapshotStore {
    fn bring_up(&self) -> BoxFuture<'_, Album> {
        Box::pin(async { Album::from(InMemorySnapshotStore::default()) })
    }
}

/// The way a [`SkewedSnapshotStore`] departs from what a snapshot store owes.
///
/// Each one is a mistake a backend could make while every byte it stored looks
/// right, and each is the mistake one `SNAP` clause exists to refuse.
#[derive(Debug, Clone, Copy)]
pub enum SnapshotFault {
    /// An aggregate the store holds no snapshot for fails its load instead of
    /// being answered that there is none.
    ///
    /// SNAP-1: an aggregate activating for the first time would then be unable
    /// to tell "nothing was ever saved for me" from "the store is broken", and
    /// the contract has it carry on from its default state.
    FailTheUnsavedAggregate,
    /// The first snapshot an aggregate ever saved is the one every later load
    /// answers with.
    ///
    /// SNAP-3: an aggregate would restore an old state and then replay from the
    /// position that state was taken at, which is the one reading that produces
    /// no error anywhere.
    TheFirstSaveWins,
    /// The sequence comes from the latest save and the payload from the first,
    /// so what is answered is a pair no single save ever wrote.
    ///
    /// SNAP-3: the worst of the two, because the sequence looks current. An
    /// aggregate would restore the old payload and then replay from beyond the
    /// events that payload is missing, losing them for good.
    TheLatestSequenceWithTheFirstPayload,
    /// One slot for every aggregate: what was saved for one is read back for
    /// all of them.
    ///
    /// SNAP-4: an aggregate would restore another aggregate's state — a store
    /// whose lookup forgot which aggregate it was asked about.
    OneSlotForEveryAggregate,
}

/// A snapshot store that keeps the contract except for the clause its fault
/// names.
///
/// It keeps the saves it was handed rather than an answer it has already chosen,
/// so each fault is a way of reading that log back and there is one copy of
/// what happened.
/// Cloneable over shared saves, for the reason [`SkewedEventStore`] states.
#[derive(Clone)]
pub struct SkewedSnapshotStore {
    fault: SnapshotFault,
    saves: Arc<Mutex<Vec<PersistedSnapshot>>>,
}

impl SkewedSnapshotStore {
    fn injecting(fault: SnapshotFault) -> Self {
        Self {
            fault,
            saves: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Vec<PersistedSnapshot>> {
        self.saves
            .lock()
            .expect("the skewed snapshot store's state lock was poisoned by a panicking holder")
    }
}

#[async_trait]
impl SnapshotStore for SkewedSnapshotStore {
    async fn save(&self, snapshot: PersistedSnapshot) -> Result<(), SnapshotError> {
        self.lock().push(snapshot);
        Ok(())
    }

    async fn load_latest(
        &self,
        aggregate_id: &AggregateId,
    ) -> Result<Option<PersistedSnapshot>, SnapshotError> {
        let saves = self.lock();
        let of_this_aggregate: Vec<&PersistedSnapshot> = saves
            .iter()
            .filter(|saved| &saved.aggregate_id == aggregate_id)
            .collect();
        let first = of_this_aggregate.first().copied();
        let latest = of_this_aggregate.last().copied();

        match self.fault {
            SnapshotFault::FailTheUnsavedAggregate => match latest {
                Some(found) => Ok(Some(found.clone())),
                None => Err(SnapshotError::Backend(
                    "this store reports an aggregate it holds no snapshot for as a failure".into(),
                )),
            },
            SnapshotFault::TheFirstSaveWins => Ok(first.cloned()),
            SnapshotFault::TheLatestSequenceWithTheFirstPayload => {
                Ok(latest.zip(first).map(|(latest, first)| PersistedSnapshot {
                    payload: first.payload.clone(),
                    ..latest.clone()
                }))
            }
            SnapshotFault::OneSlotForEveryAggregate => Ok(saves.last().cloned()),
        }
    }
}

/// A snapshot store under test that brings up a backend breaking exactly the one
/// clause `fault` names.
pub struct FaultySnapshotStore {
    fault: SnapshotFault,
}

impl FaultySnapshotStore {
    pub fn injecting(fault: SnapshotFault) -> Self {
        Self { fault }
    }
}

impl SnapshotStoreUnderTest for FaultySnapshotStore {
    fn bring_up(&self) -> BoxFuture<'_, Album> {
        let fault = self.fault;
        Box::pin(async move { Album::from(SkewedSnapshotStore::injecting(fault)) })
    }
}

/// The reference implementation the `OFS` clauses are written against.
///
/// The associated store type is the concrete one rather than a `dyn` handle:
/// `OffsetStore::Tx` is an associated type, and a transaction is the one
/// thing an offset store's own backend decides.
pub struct ReferenceOffsetStore;

impl OffsetStoreUnderTest for ReferenceOffsetStore {
    type Store = InMemoryOffsetStore;

    fn bring_up(&self) -> BoxFuture<'_, Self::Store> {
        Box::pin(async { InMemoryOffsetStore::default() })
    }
}

/// The way a [`SkewedOffsetStore`] departs from what an offset store
/// owes.
#[derive(Debug, Clone, Copy)]
pub enum OffsetFault {
    /// A scope nothing has saved for reads as a position of zero rather than as
    /// no position at all.
    ///
    /// OFS-1: the one answer that cannot be told from a consumer that really
    /// did stop at zero, which is why the clause asks for `None`.
    ReadTheUnsavedScopeAsZero,
    /// Only the sequence immediately after the stored one is taken; a save that
    /// skips numbers is refused.
    ///
    /// OFS-3: a consumer following a store-wide order moves over records it has
    /// no handler for, so the position it saves is not the one before it plus
    /// one. A store that required the next number would refuse every one of
    /// them.
    TakeOnlyTheSequenceAfterTheStoredOne,
    /// One slot for every scope: a position saved under one key is read back
    /// under every other.
    ///
    /// OFS-5: each consumer would resume from wherever another one had got to —
    /// a lookup that forgot the key it was given.
    OneSlotForEveryScope,
    /// A key inside the framework's reserved namespace is refused rather than
    /// stored like any other.
    ///
    /// OFS-5: the rendered key is the whole of what a store is told, and a
    /// framework-owned consumer renders inside that namespace on purpose. A
    /// store that read the namespace as something to validate would keep the
    /// framework's own consumers from recording anything.
    RefuseAKeyInsideTheReservedNamespace,
}

/// Where a store with one slot for everything keeps the only position it has.
const THE_ONLY_ROW: &str = "the one row this store has";

/// An offset store that keeps the contract except for the clause its fault
/// names.
/// Cloneable over shared positions, for the reason [`SkewedEventStore`]
/// states.
#[derive(Clone)]
pub struct SkewedOffsetStore {
    fault: OffsetFault,
    positions: Arc<Mutex<HashMap<String, u64>>>,
}

impl SkewedOffsetStore {
    fn injecting(fault: OffsetFault) -> Self {
        Self {
            fault,
            positions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, u64>> {
        self.positions
            .lock()
            .expect("the skewed offset store's state lock was poisoned by a panicking holder")
    }

    /// The key this store files a position under.
    ///
    /// Everything but the one-slot fault files the key the scope rendered, which
    /// is the whole of what the contract says a store is told.
    fn key_of(&self, scope: &OffsetScope) -> String {
        match self.fault {
            OffsetFault::OneSlotForEveryScope => THE_ONLY_ROW.to_owned(),
            _ => scope.as_str().to_owned(),
        }
    }
}

#[async_trait]
impl OffsetStore for SkewedOffsetStore {
    type Tx = ();

    async fn load(&self, scope: &OffsetScope) -> Result<Option<u64>, OffsetError> {
        let key = self.key_of(scope);
        let saved = self.lock().get(&key).copied();

        match self.fault {
            OffsetFault::ReadTheUnsavedScopeAsZero => Ok(Some(saved.unwrap_or(0))),
            _ => Ok(saved),
        }
    }

    async fn save(
        &self,
        scope: &OffsetScope,
        sequence: u64,
        _tx: Option<&mut Self::Tx>,
    ) -> Result<(), OffsetError> {
        if matches!(
            self.fault,
            OffsetFault::RefuseAKeyInsideTheReservedNamespace
        ) && is_within_reserved_namespace(scope.as_str())
        {
            return Err(OffsetError::Backend(
                "this store refuses an offset key inside the framework's reserved namespace".into(),
            ));
        }

        let key = self.key_of(scope);
        let mut positions = self.lock();

        if matches!(
            self.fault,
            OffsetFault::TakeOnlyTheSequenceAfterTheStoredOne
        ) {
            if let Some(held) = positions.get(&key).copied() {
                if sequence != held + 1 {
                    return Err(OffsetError::Backend(
                        format!(
                            "this store takes only the sequence after {held}, and was handed \
                             {sequence}"
                        )
                        .into(),
                    ));
                }
            }
        }

        positions.insert(key, sequence);
        Ok(())
    }
}

/// An offset store under test that brings up a backend breaking exactly the
/// one clause `fault` names.
pub struct FaultyOffsetStore {
    fault: OffsetFault,
}

impl FaultyOffsetStore {
    pub fn injecting(fault: OffsetFault) -> Self {
        Self { fault }
    }
}

impl OffsetStoreUnderTest for FaultyOffsetStore {
    type Store = SkewedOffsetStore;

    fn bring_up(&self) -> BoxFuture<'_, Self::Store> {
        let fault = self.fault;
        Box::pin(async move { SkewedOffsetStore::injecting(fault) })
    }
}

/// The reference store the `PRV` clauses are written against.
///
/// [`InMemoryStore`] is the assembly the framework ships: an in-memory store
/// for every slot, with a handle of its own for each offset slot. The
/// single-table shape — both slots named with one handle — is what
/// [`ProviderWithCollidingOffsetSlots`] is assembled in, so the
/// independence clause is read against both.
pub struct ReferenceProvider;

impl ProviderUnderTest for ReferenceProvider {
    type Store = InMemoryStore;

    fn bring_up(&self) -> BoxFuture<'_, Self::Store> {
        Box::pin(async { InMemoryStore::default() })
    }
}

/// A store that answers every `events` call with a store of its own.
///
/// PRV-1: the handles a caller takes would then name a world each, so an append
/// made through one is invisible through the other and nothing reports it. That
/// is why the clause reads a record back through a second handle rather than
/// comparing the handles themselves — a store handing out two handles onto one
/// region and one handing out two regions are indistinguishable until something
/// is written.
#[derive(Clone, Default)]
pub struct AStoreOfItsOwnPerCall;

impl Store for AStoreOfItsOwnPerCall {
    /// The fault: every call erases a backend of its own.
    fn events(&self) -> Journal {
        Journal::from(InMemoryEventStore::default())
    }
}

/// A store under test whose journal slot splits the world.
pub struct FaultyProvider;

impl ProviderUnderTest for FaultyProvider {
    type Store = AStoreOfItsOwnPerCall;

    fn bring_up(&self) -> BoxFuture<'_, Self::Store> {
        Box::pin(async { AStoreOfItsOwnPerCall })
    }
}

/// A store whose handle answers consistently and whose *clone* names a region
/// of its own.
///
/// PRV-1 across clones: a store handed to two wiring points by cloning the
/// handle is the ordinary way a deployment distributes it, so a `Clone` that
/// built fresh state splits the deployment in two with nothing reported. Every
/// clause that takes a slot twice from one handle passes this store, which is
/// why the clause reads across a clone as well.
pub struct AStoreWhoseCloneSplitsTheWorld {
    events: Journal,
}

impl Default for AStoreWhoseCloneSplitsTheWorld {
    fn default() -> Self {
        Self {
            events: Journal::from(InMemoryEventStore::default()),
        }
    }
}

/// The fault: what should copy the handle constructs a new region instead.
impl Clone for AStoreWhoseCloneSplitsTheWorld {
    fn clone(&self) -> Self {
        Self::default()
    }
}

impl Store for AStoreWhoseCloneSplitsTheWorld {
    fn events(&self) -> Journal {
        self.events.clone()
    }
}

/// A store under test whose clone splits the world.
pub struct ProviderWithCloneThatSplitsTheWorld;

impl ProviderUnderTest for ProviderWithCloneThatSplitsTheWorld {
    type Store = AStoreWhoseCloneSplitsTheWorld;

    fn bring_up(&self) -> BoxFuture<'_, Self::Store> {
        Box::pin(async { AStoreWhoseCloneSplitsTheWorld::default() })
    }
}

/// What [`ProviderWithCollidingOffsetSlots`] brings up: both offset
/// slots named with a store that keeps one position for every key.
pub type CollidingOffsets = CustomStore<Unprovided, SkewedOffsetStore, SkewedOffsetStore>;

/// A store whose two offset slots reach a store with one slot for every
/// scope.
///
/// PRV-4: naming both slots with one handle is allowed, and what makes it
/// allowed is that each consumer files under a key of its own. A store that
/// forgot the key it was given has a delivery host resume from a projection's
/// position, so the slots of an assembly over one are not independent however
/// they were named.
pub struct ProviderWithCollidingOffsetSlots;

impl ProviderUnderTest for ProviderWithCollidingOffsetSlots {
    type Store = CollidingOffsets;

    fn bring_up(&self) -> BoxFuture<'_, Self::Store> {
        Box::pin(async {
            CustomStore::builder()
                .eventstore(InMemoryEventStore::default())
                .offsets(SkewedOffsetStore::injecting(
                    OffsetFault::OneSlotForEveryScope,
                ))
                .build()
        })
    }
}
