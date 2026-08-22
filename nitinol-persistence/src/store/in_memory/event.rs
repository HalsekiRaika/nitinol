use std::collections::{HashMap, HashSet};
use std::pin::Pin;
use std::sync::Mutex;
use std::task::{Context, Poll};

use async_trait::async_trait;
use futures_core::Stream;

use crate::error::{AppendError, LoadError};
use crate::event::{AppendingEvent, LoadedEvent};
use crate::query::{AppendOutcome, LoadQuery};
use crate::store::event_store::{EventStore, EventStream};

struct EventStoreState {
    events: HashMap<String, Vec<LoadedEvent>>,
    next_global_sequence: u64,
}

impl Default for EventStoreState {
    fn default() -> Self {
        Self {
            events: HashMap::new(),
            // global_sequence starts at 1
            next_global_sequence: 1,
        }
    }
}

/// In-memory reference implementation of [`EventStore`].
///
/// This type is the reference implementation of the optimistic-concurrency-control
/// contract every backend must reproduce: unique `(stream, sequence)`, a genesis
/// conflict meaning "already created", `global_sequence` monotonicity with
/// commit-unit atomic visibility, and no internal retry.  The clauses, their
/// labels and their exact wording live on [`EventStore::append`].
///
/// The contract itself is fixed by the tests in
/// `nitinol-persistence/tests/event_store_occ.rs`, which run against this
/// type.  Those tests — not this implementation's incidental behaviour — are
/// what a third-party backend has to satisfy.
///
/// Intended for tests and examples; not for production use.
pub struct InMemoryEventStore {
    state: Mutex<EventStoreState>,
}

impl Default for InMemoryEventStore {
    fn default() -> Self {
        Self {
            state: Mutex::new(EventStoreState::default()),
        }
    }
}

#[async_trait]
impl EventStore for InMemoryEventStore {
    async fn append(
        &self,
        key: &str,
        events: Vec<AppendingEvent>,
    ) -> Result<AppendOutcome, AppendError> {
        let mut state = self
            .state
            .lock()
            .expect("in-memory event state lock was poisoned by a panicking holder");

        // Empty batch: no-op — return current max sequence (0 if no events exist yet)
        if events.is_empty() {
            let stream_version = state
                .events
                .get(key)
                .and_then(|stored| stored.iter().map(|e| e.sequence).max())
                .unwrap_or(0);
            return Ok(AppendOutcome {
                assigned_sequences: vec![],
                stream_version,
            });
        }

        // Collect existing sequences; seen.insert returns false on duplicate,
        // detecting both store conflicts and intra-batch duplicates in one pass.
        let mut seen: HashSet<u64> = state
            .events
            .get(key)
            .map(|stored| stored.iter().map(|e| e.sequence).collect())
            .unwrap_or_default();

        for event in &events {
            if !seen.insert(event.sequence) {
                return Err(AppendError::SequenceConflict(key.to_owned()));
            }
        }

        // No conflict: assign global_sequence and insert (all-or-nothing)
        let mut assigned_sequences = Vec::with_capacity(events.len());
        let mut stream_version = 0u64;

        for event in events {
            let global_sequence = state.next_global_sequence;
            state.next_global_sequence += 1;

            stream_version = stream_version.max(event.sequence);
            assigned_sequences.push(global_sequence);

            state
                .events
                .entry(key.to_owned())
                .or_default()
                .push(LoadedEvent {
                    stream_key: key.to_owned(),
                    sequence: event.sequence,
                    global_sequence,
                    event_type: event.event_type,
                    payload: event.payload,
                    occurred_at: event.occurred_at,
                });
        }

        Ok(AppendOutcome {
            assigned_sequences,
            stream_version,
        })
    }

    async fn load(&self, query: LoadQuery) -> Result<EventStream<'_>, LoadError> {
        let state = self
            .state
            .lock()
            .expect("in-memory event state lock was poisoned by a panicking holder");

        let mut matching: Vec<LoadedEvent> = state
            .events
            .values()
            .flat_map(|events| events.iter().cloned())
            .filter(|event| {
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
                if let Some(from_stream_seq) = query.from_stream_sequence {
                    if event.sequence < from_stream_seq {
                        return false;
                    }
                }
                true
            })
            .collect();

        // Return events in ascending global_sequence order
        matching.sort_unstable_by_key(|e| e.global_sequence);

        if let Some(limit) = query.limit {
            matching.truncate(limit);
        }

        drop(state);

        let items: Vec<Result<LoadedEvent, LoadError>> = matching.into_iter().map(Ok).collect();

        Ok(Box::pin(OwnedEventStream {
            items: items.into_iter(),
        }))
    }
}

/// Provides a Vec of owned LoadedEvents as a Stream without holding external borrows.
struct OwnedEventStream {
    items: std::vec::IntoIter<Result<LoadedEvent, LoadError>>,
}

impl Stream for OwnedEventStream {
    type Item = Result<LoadedEvent, LoadError>;

    fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Poll::Ready(self.get_mut().items.next())
    }
}
