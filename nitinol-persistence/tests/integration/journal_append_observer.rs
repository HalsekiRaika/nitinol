//! A journal observed by an [`AppendObserver`] tells it about every append the
//! backend accepted, once, and about nothing the backend refused.
//!
//! ```gherkin
//!   Scenario: an accepted append is reported once, as the backend answered it
//!     Given a journal observed by a recording observer, over a backend that
//!           already assigned a global position to another stream
//!     When one append of three events of two types is accepted by the backend
//!     Then the observer has been told once, with the key, the outcome the
//!          journal handed back and the type of every event in the order
//!          appended
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
//!
//!   Scenario: the appender waits for the observer
//!     Given a journal observed by an observer that holds until it is let go
//!     When an append is accepted and the observer is holding
//!     Then the append has not returned
//!     And once the observer is let go, the append returns what was accepted
//! ```
//!
//! The observer is what the framework hangs its append notification on: a
//! journal handed out once is cloned into every activation that appends, so an
//! observer the clone dropped would be an append nobody hears about. The
//! refusal clause is the other half of "only what is in the journal": a
//! notification for a refused append would point a consumer at a record that
//! is not there. The waiting clause is what lets an observer hand the fact on
//! with an `.await` rather than dropping it when a queue is full: the append it
//! reports does not return until the observer has.
//!
//! Run with:
//! `cargo test -p nitinol-persistence --test integration journal_append_observer::`.

use std::future::{poll_fn, Future};
use std::pin::pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use async_trait::async_trait;
use bytes::Bytes;
use jiff::Timestamp;
use nitinol_persistence::error::AppendError;
use nitinol_persistence::store::{AppendObserver, InMemoryEventStore, Journal};
use nitinol_persistence::{
    AggregateId, AppendOutcome, AppendingEvent, EventType, Family, TypeName,
};

// Named so that sorting the types by name would put them in another order than
// the one appended.
const OPENED: EventType = EventType::new(
    Family::new("journal.append.observer"),
    TypeName::new("Opened"),
);
const MOVED: EventType = EventType::new(
    Family::new("journal.append.observer"),
    TypeName::new("Moved"),
);

const RECORD_LOCK: &str = "the observer's record lock guards only a push or a copy";
const GATE_LOCK: &str = "the gate's lock guards only a flag and a waker";

fn an_event(sequence: u64, event_type: EventType, payload: &'static [u8]) -> AppendingEvent {
    AppendingEvent {
        sequence,
        event_type,
        payload: Bytes::from_static(payload),
        occurred_at: Timestamp::now(),
    }
}

/// What one call to [`AppendObserver::appended`] was handed, in a shape that
/// compares by value.
#[derive(Debug, Clone, PartialEq)]
struct Told {
    key: String,
    assigned_sequences: Vec<u64>,
    stream_version: u64,
    types: Vec<EventType>,
}

impl Told {
    fn of(key: &str, outcome: &AppendOutcome, types: &[EventType]) -> Self {
        Self {
            key: key.to_owned(),
            assigned_sequences: outcome.assigned_sequences.clone(),
            stream_version: outcome.stream_version,
            types: types.to_vec(),
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

#[async_trait]
impl AppendObserver for RecordingObserver {
    async fn appended(&self, key: &str, outcome: &AppendOutcome, types: &[EventType]) {
        self.told
            .lock()
            .expect(RECORD_LOCK)
            .push(Told::of(key, outcome, types));
    }
}

fn observed_journal() -> (Journal, Arc<RecordingObserver>) {
    observed_journal_over(InMemoryEventStore::default())
}

fn observed_journal_over(store: InMemoryEventStore) -> (Journal, Arc<RecordingObserver>) {
    let observer = Arc::new(RecordingObserver::default());
    let journal =
        Journal::from(store).observed_by(Arc::clone(&observer) as Arc<dyn AppendObserver>);
    (journal, observer)
}

// Scenario: an accepted append is reported once, as the backend answered it

#[tokio::test]
async fn an_accepted_append_is_reported_once_with_its_key_outcome_and_types() {
    // Given
    // An append to another stream first, through a journal nobody observes,
    // so the global positions the observed append is given are not its
    // stream sequences: an outcome rebuilt from the events would differ from
    // the one the backend answered with.
    let store = InMemoryEventStore::default();
    Journal::from(store.clone())
        .append(
            AggregateId::new("journal.append.observer.elsewhere").as_str(),
            vec![an_event(1, OPENED, b"elsewhere")],
        )
        .await
        .expect("appending to an empty stream must be accepted");
    let (journal, observer) = observed_journal_over(store);
    let stream = AggregateId::new("journal.append.observer.accepted");
    let events = vec![
        an_event(1, OPENED, b"first"),
        an_event(2, MOVED, b"second"),
        an_event(3, OPENED, b"third"),
    ];

    // When
    let outcome = journal
        .append(stream.as_str(), events)
        .await
        .expect("appending fresh sequences to an empty stream must be accepted");

    // Then
    assert_eq!(
        observer.told(),
        vec![Told::of(
            stream.as_str(),
            &outcome,
            &[OPENED, MOVED, OPENED]
        )],
        "an accepted append must reach the observer exactly once, carrying the key it was \
         made with, the outcome the journal handed back, and the type of every event in the \
         order appended — repeats kept"
    );
}

// Scenario: a refused append is not reported

#[tokio::test]
async fn a_refused_append_is_not_reported() {
    // Given
    let (journal, observer) = observed_journal();
    let stream = AggregateId::new("journal.append.observer.refused");
    let accepted = journal
        .append(stream.as_str(), vec![an_event(1, OPENED, b"accepted")])
        .await
        .expect("the first append of a stream must be accepted");

    // When
    let refused = journal
        .append(stream.as_str(), vec![an_event(1, MOVED, b"conflicting")])
        .await;

    // Then
    assert!(
        matches!(refused, Err(AppendError::SequenceConflict(_))),
        "re-appending a taken sequence is the backend's sequence conflict, handed back \
         unchanged; got {refused:?}"
    );
    assert_eq!(
        observer.told(),
        vec![Told::of(stream.as_str(), &accepted, &[OPENED])],
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

    // When
    let outcome = handed_on
        .append(
            stream.as_str(),
            vec![an_event(1, OPENED, b"through the clone")],
        )
        .await
        .expect("appending to an empty stream through the clone must be accepted");

    // Then
    assert_eq!(
        observer.told(),
        vec![Told::of(stream.as_str(), &outcome, &[OPENED])],
        "a journal's clone must carry its observer: the framework appends through clones, \
         and an observer the clone dropped would never hear of those appends"
    );
}

// Scenario: the appender waits for the observer

#[derive(Default)]
struct GateState {
    entered: bool,
    open: bool,
    waiting: Option<Waker>,
}

/// An observer that, once told, holds until [`open`](HoldingObserver::open)
/// lets it go.
#[derive(Default)]
struct HoldingObserver {
    gate: Mutex<GateState>,
}

impl HoldingObserver {
    fn entered(&self) -> bool {
        self.gate.lock().expect(GATE_LOCK).entered
    }

    fn open(&self) {
        let waiting = {
            let mut gate = self.gate.lock().expect(GATE_LOCK);
            gate.open = true;
            gate.waiting.take()
        };
        if let Some(waker) = waiting {
            waker.wake();
        }
    }
}

#[async_trait]
impl AppendObserver for HoldingObserver {
    async fn appended(&self, _key: &str, _outcome: &AppendOutcome, _types: &[EventType]) {
        poll_fn(|cx| {
            let mut gate = self.gate.lock().expect(GATE_LOCK);
            gate.entered = true;
            if gate.open {
                Poll::Ready(())
            } else {
                gate.waiting = Some(cx.waker().clone());
                Poll::Pending
            }
        })
        .await;
    }
}

#[tokio::test]
async fn an_append_does_not_return_until_its_observer_has() {
    // Given
    let observer = Arc::new(HoldingObserver::default());
    let journal = Journal::from(InMemoryEventStore::default())
        .observed_by(Arc::clone(&observer) as Arc<dyn AppendObserver>);
    let stream = AggregateId::new("journal.append.observer.held");
    let mut append = pin!(journal.append(stream.as_str(), vec![an_event(1, OPENED, b"held")]));

    // When
    // The in-memory backend answers without suspending, so one poll carries
    // the append through the backend and into the observer.
    let first_poll = append
        .as_mut()
        .poll(&mut Context::from_waker(Waker::noop()));

    // Then
    assert!(
        observer.entered(),
        "the accepted append must have reached the observer within the appender's own call"
    );
    assert!(
        first_poll.is_pending(),
        "an append must not return while its observer is still holding: the observer runs \
         on the appender's task, inside its append; got {first_poll:?}"
    );

    observer.open();
    let outcome = append
        .await
        .expect("the append the observer held must be accepted once it is let go");
    assert_eq!(
        outcome.stream_version, 1,
        "once the observer is let go, the append returns the outcome the backend assigned"
    );
}
