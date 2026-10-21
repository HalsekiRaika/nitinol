//! A journal's clone is the backend's clone, so a backend that counts, records
//! or injects a failure carries that state through the erasure.
//!
//! ```gherkin
//!   Scenario: a recording backend's records survive the clone
//!     Given a journal over a store that records every append it accepts
//!     When one append goes through the journal and another through its clone
//!     Then both are in the one record the test holds
//!
//!   Scenario: an injected failure is not reset by the clone
//!     Given a journal over a store that refuses its first append
//!     When the first append goes through the journal and the second through
//!          its clone
//!     Then the first is refused and the second is accepted
//! ```
//!
//! This is the clause the framework's test-only stores rest on, and it is the
//! one a hand-written `Clone` breaks. A backend holding its counter, its record
//! or its failure flag by value cannot derive `Clone` at all — the atomics and
//! the mutex are not `Clone` — so the tempting repair is a `Clone` that
//! constructs fresh state, which compiles and splits the world: every handle the
//! framework distributes gets a counter of its own, a store that was supposed to
//! fail once fails once *per consumer*, and the test that named it observes
//! whichever half it kept.
//!
//! Nothing reports that. Both halves answer every call, the appends succeed, and
//! the assertions the fixture exists for read an empty record or an
//! already-reset flag. The two clauses here are the pair that excludes it: the
//! recording clause fails when a clone records elsewhere, and the failure clause
//! fails when a clone starts again from "not yet failed".
//!
//! Run with:
//! `cargo test -p nitinol-persistence --test journal_clone_shares_backend_state`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::Bytes;
use jiff::Timestamp;
use nitinol_persistence::error::{AppendError, LoadError};
use nitinol_persistence::store::{EventStore, EventStream, InMemoryEventStore, Journal};
use nitinol_persistence::{
    AggregateId, AppendOutcome, AppendingEvent, EventType, Family, LoadQuery, LoadedEvent, TypeName,
};

const A_FACT: EventType = EventType::new(Family::new("journal.clone"), TypeName::new("Fact"));

fn a_fact(sequence: u64) -> AppendingEvent {
    AppendingEvent {
        sequence,
        event_type: A_FACT,
        payload: Bytes::from_static(b"payload"),
        occurred_at: Timestamp::now(),
    }
}

/// What every handle onto a [`RecordingStore`] writes into.
type Accepted = Arc<Mutex<Vec<String>>>;

/// A store that records the key of every append it accepts.
///
/// The record is the shared state: a clone that recorded elsewhere would be a
/// second store wearing the same name.
#[derive(Clone)]
struct RecordingStore {
    inner: InMemoryEventStore,
    accepted: Accepted,
}

impl RecordingStore {
    fn over(accepted: &Accepted) -> Self {
        Self {
            inner: InMemoryEventStore::default(),
            accepted: Arc::clone(accepted),
        }
    }
}

#[async_trait]
impl EventStore for RecordingStore {
    async fn append(
        &self,
        key: &str,
        events: Vec<AppendingEvent>,
    ) -> Result<AppendOutcome, AppendError> {
        let outcome = self.inner.append(key, events).await?;
        self.accepted
            .lock()
            .expect("the recording store's record must not be poisoned")
            .push(key.to_owned());
        Ok(outcome)
    }

    async fn load(&self, query: LoadQuery) -> Result<EventStream<'_>, LoadError> {
        self.inner.load(query).await
    }
}

/// A store that refuses its first append and accepts every one after it.
///
/// The flag is the shared state: a clone that started again from "not yet
/// failed" would refuse once per handle rather than once per store.
#[derive(Clone, Default)]
struct FailOnceStore {
    has_failed: Arc<AtomicBool>,
}

#[async_trait]
impl EventStore for FailOnceStore {
    async fn append(
        &self,
        _key: &str,
        _events: Vec<AppendingEvent>,
    ) -> Result<AppendOutcome, AppendError> {
        if self.has_failed.swap(true, Ordering::SeqCst) {
            Ok(AppendOutcome {
                assigned_sequences: vec![],
                stream_version: 0,
            })
        } else {
            Err(AppendError::Backend("injected failure".into()))
        }
    }

    async fn load(&self, _query: LoadQuery) -> Result<EventStream<'_>, LoadError> {
        let empty = futures_util::stream::empty::<Result<LoadedEvent, LoadError>>();
        Ok(Box::pin(empty))
    }
}

/// Given a journal over a recording store, When one append goes through it and
/// another through its clone, Then the one record the test holds has both.
#[tokio::test]
async fn a_clone_of_a_journal_records_into_the_backends_own_record() {
    // Given
    let accepted: Accepted = Arc::new(Mutex::new(Vec::new()));
    let journal = Journal::from(RecordingStore::over(&accepted));
    let handed_on = journal.clone();
    let first = AggregateId::new("journal.clone.recording.first");
    let second = AggregateId::new("journal.clone.recording.second");

    // When
    journal
        .append(first.as_str(), vec![a_fact(1)])
        .await
        .expect("the append through the original journal must be accepted");
    handed_on
        .append(second.as_str(), vec![a_fact(1)])
        .await
        .expect("the append through the cloned journal must be accepted");

    // Then
    assert_eq!(
        accepted
            .lock()
            .expect("the recording store's record must not be poisoned")
            .clone(),
        vec![first.as_str().to_owned(), second.as_str().to_owned()],
        "a journal's clone is the backend's clone, so both handles record into \
         the one record the store holds; a clone that built state of its own \
         would leave the second append out of this and nothing would report it"
    );
}

/// Given a journal over a store that refuses its first append, When the first
/// goes through the journal and the second through its clone, Then the refusal
/// is not handed out a second time.
#[tokio::test]
async fn a_clone_of_a_journal_does_not_reset_an_injected_failure() {
    // Given
    let journal = Journal::from(FailOnceStore::default());
    let handed_on = journal.clone();
    let stream = AggregateId::new("journal.clone.fail-once");

    // When
    let refused = journal.append(stream.as_str(), vec![a_fact(1)]).await;
    let accepted = handed_on.append(stream.as_str(), vec![a_fact(2)]).await;

    // Then
    assert!(
        matches!(refused, Err(AppendError::Backend(_))),
        "the store's first append is the injected backend failure, and that is \
         the error the journal hands back unchanged; got {refused:?}"
    );
    assert!(
        accepted.is_ok(),
        "the clone shares the flag the first append set, so the second append \
         is the accepted one: a clone starting again from `not yet failed` would \
         refuse here and turn a once-per-store injection into a \
         once-per-consumer one; got {accepted:?}"
    );
}
