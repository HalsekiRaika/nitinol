//! A journal observed by an [`AppendObserver`] tells it about every append the
//! backend accepted, once, and about nothing the backend refused.
//!
//! ```gherkin
//!   Scenario: an accepted append is reported once, as the backend answered it
//!     Given a journal observed by a recording observer
//!     When one append of two events is accepted by the backend
//!     Then the observer has been told once, with the key, the events and the
//!          outcome the journal handed back
//!
//!   Scenario: a refused append is not reported
//!     Given a journal observed by a recording observer, over a stream that
//!           already holds sequence 1
//!     When an append of sequence 1 again is refused with a sequence conflict
//!     Then the observer has been told only about the accepted first append
//!
//!   Scenario: the observer travels with the journal's clone
//!     Given a journal observed by a recording observer
//!     When an append goes through a clone of it
//!     Then the observer has been told about that append
//! ```
//!
//! The observer is what the framework hangs its append notification on: a
//! journal handed out once is cloned into every activation that appends, so an
//! observer the clone dropped would be an append nobody hears about. The
//! refusal clause is the other half of "only what is in the journal": a
//! notification for a refused append would point a consumer at a record that
//! is not there.
//!
//! Run with:
//! `cargo test -p nitinol-persistence --test integration journal_append_observer::`.

use std::sync::{Arc, Mutex};

use bytes::Bytes;
use jiff::Timestamp;
use nitinol_persistence::error::AppendError;
use nitinol_persistence::store::{AppendObserver, InMemoryEventStore, Journal};
use nitinol_persistence::{
    AggregateId, AppendOutcome, AppendingEvent, EventType, Family, TypeName,
};

const OBSERVED: EventType = EventType::new(
    Family::new("journal.append.observer"),
    TypeName::new("Observed"),
);

const RECORD_LOCK: &str = "the observer's record lock guards only a push or a copy";

fn an_event(sequence: u64, payload: &'static [u8]) -> AppendingEvent {
    AppendingEvent {
        sequence,
        event_type: OBSERVED,
        payload: Bytes::from_static(payload),
        occurred_at: Timestamp::now(),
    }
}

/// What one call to [`AppendObserver::appended`] was handed, in a shape that
/// compares by value.
#[derive(Debug, Clone, PartialEq)]
struct Told {
    key: String,
    /// `(sequence, event_type, payload)` of each event, in the order handed.
    events: Vec<(u64, EventType, Bytes)>,
    assigned_sequences: Vec<u64>,
    stream_version: u64,
}

impl Told {
    fn of(key: &str, events: &[AppendingEvent], outcome: &AppendOutcome) -> Self {
        Self {
            key: key.to_owned(),
            events: events
                .iter()
                .map(|event| (event.sequence, event.event_type, event.payload.clone()))
                .collect(),
            assigned_sequences: outcome.assigned_sequences.clone(),
            stream_version: outcome.stream_version,
        }
    }
}

/// Records every call it receives.
#[derive(Default)]
struct RecordingObserver {
    told: Mutex<Vec<Told>>,
}

impl RecordingObserver {
    fn told(&self) -> Vec<Told> {
        self.told.lock().expect(RECORD_LOCK).clone()
    }
}

impl AppendObserver for RecordingObserver {
    fn appended(&self, key: &str, events: &[AppendingEvent], outcome: &AppendOutcome) {
        self.told
            .lock()
            .expect(RECORD_LOCK)
            .push(Told::of(key, events, outcome));
    }
}

fn observed_journal() -> (Journal, Arc<RecordingObserver>) {
    let observer = Arc::new(RecordingObserver::default());
    let journal = Journal::from(InMemoryEventStore::default())
        .observed_by(Arc::clone(&observer) as Arc<dyn AppendObserver>);
    (journal, observer)
}

// Scenario: an accepted append is reported once, as the backend answered it

#[tokio::test]
async fn an_accepted_append_is_reported_once_with_what_the_journal_answered() {
    // Given
    let (journal, observer) = observed_journal();
    let stream = AggregateId::new("journal.append.observer.accepted");
    let events = vec![an_event(1, b"first"), an_event(2, b"second")];
    let expected_events = events.clone();

    // When
    let outcome = journal
        .append(stream.as_str(), events)
        .await
        .expect("appending fresh sequences to an empty stream must be accepted");

    // Then
    assert_eq!(
        observer.told(),
        vec![Told::of(stream.as_str(), &expected_events, &outcome)],
        "an accepted append must reach the observer exactly once, carrying the key and \
         events it was made with and the outcome the journal handed back"
    );
}

// Scenario: a refused append is not reported

#[tokio::test]
async fn a_refused_append_is_not_reported() {
    // Given
    let (journal, observer) = observed_journal();
    let stream = AggregateId::new("journal.append.observer.refused");
    let accepted_events = vec![an_event(1, b"accepted")];
    let accepted = journal
        .append(stream.as_str(), accepted_events.clone())
        .await
        .expect("the first append of a stream must be accepted");

    // When
    let refused = journal
        .append(stream.as_str(), vec![an_event(1, b"conflicting")])
        .await;

    // Then
    assert!(
        matches!(refused, Err(AppendError::SequenceConflict(_))),
        "re-appending a taken sequence is the backend's sequence conflict, handed back \
         unchanged; got {refused:?}"
    );
    assert_eq!(
        observer.told(),
        vec![Told::of(stream.as_str(), &accepted_events, &accepted)],
        "only the accepted append may reach the observer: a refused one put nothing in the \
         journal to be told about"
    );
}

// Scenario: the observer travels with the journal's clone

#[tokio::test]
async fn an_append_through_a_clone_reaches_the_observer() {
    // Given
    let (journal, observer) = observed_journal();
    let handed_on = journal.clone();
    let stream = AggregateId::new("journal.append.observer.clone");
    let events = vec![an_event(1, b"through the clone")];
    let expected_events = events.clone();

    // When
    let outcome = handed_on
        .append(stream.as_str(), events)
        .await
        .expect("appending to an empty stream through the clone must be accepted");

    // Then
    assert_eq!(
        observer.told(),
        vec![Told::of(stream.as_str(), &expected_events, &outcome)],
        "a journal's clone must carry its observer: the framework appends through clones, \
         and an observer the clone dropped would never hear of those appends"
    );
}
