//! What a caller brings to the kit: a store to put under test, and a runtime to
//! run the concurrent clauses on.
//!
//! [`ReferenceStore`] and [`TokioExecutor`] are the whole of the wiring a
//! provider writes. [`FaultyStore`] is the kit's own self-check: it brings up a
//! store whose numbering order and visibility order come apart on purpose, so
//! the clauses can be shown to catch a backend that breaks the contract instead
//! of merely passing against one that keeps it.
//!
//! The doubles live in this crate rather than in the kit's `src/` for two
//! reasons: they are not part of what the kit publishes, and a store defined
//! outside the kit's own crate is the direct evidence that a provider's
//! `EventStore` can be handed to the public API.
//!
//! Each test binary compiles this module in full but uses only the subset it
//! needs, so per-binary dead code is expected.

#![allow(dead_code)]

use std::collections::HashSet;
use std::sync::{Arc, Mutex, MutexGuard};

use async_trait::async_trait;
use futures_core::future::BoxFuture;
use futures_util::stream;
use nitinol_persistence::error::{AppendError, LoadError};
use nitinol_persistence::store::{EventStore, EventStream, InMemoryEventStore};
use nitinol_persistence::{AppendOutcome, AppendingEvent, LoadQuery, LoadedEvent};
use nitinol_store_conformance::{Executor, StoreUnderTest};

/// The reference implementation the OCC clauses are written against.
pub struct ReferenceStore;

impl StoreUnderTest for ReferenceStore {
    fn bring_up(&self) -> BoxFuture<'_, Arc<dyn EventStore>> {
        Box::pin(async { Arc::new(InMemoryEventStore::default()) as Arc<dyn EventStore> })
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
    /// checkpoint reader that has passed `N` never asks for anything at or
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
    fn bring_up(&self) -> BoxFuture<'_, Arc<dyn EventStore>> {
        let fault = self.fault;
        Box::pin(async move { Arc::new(SkewedEventStore::injecting(fault)) as Arc<dyn EventStore> })
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
pub struct SkewedEventStore {
    fault: Fault,
    state: Mutex<SkewedState>,
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
            state: Mutex::new(SkewedState {
                visible: Vec::new(),
                withheld: Vec::new(),
                hidden_once: HashSet::new(),
                // global_sequence starts at 1, as it does in the reference
                // implementation.
                next_global_sequence: 1,
                highest_global_sequence_a_load_returned: 0,
            }),
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
