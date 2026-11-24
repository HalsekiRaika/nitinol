//! Where the journal ends, as the reference implementation answers it.
//!
//! ```gherkin
//!   Scenario: a store nothing was appended to has no head
//!     Given an in-memory store nothing has been appended to
//!     When its head is asked for, directly and through a journal over it
//!     Then both answer that there is none
//!
//!   Scenario: the head is the last global_sequence assigned
//!     Given a single append to one stream and a batch to another
//!     When the head is asked for
//!     Then it is the last global_sequence the batch was assigned
//!
//!   Scenario: a refused append leaves the head where it was
//!     Given a store holding a batch
//!     When an append to another stream is refused as a sequence conflict
//!     Then the head is still the last global_sequence the batch was assigned
//!
//!   Scenario: a journal answers with its backend's head
//!     Given a journal over a store appended to through another handle
//!     When the journal's head is asked for
//!     Then it is the backend's own
//! ```
//!
//! `HEAD-1` and `HEAD-2` are fixed for every backend by
//! `nitinol-store-conformance`. What is pinned here is what those clauses leave
//! open and the reference implementation decides exactly: a store that has
//! assigned nothing answers `None` rather than a position of zero, and what it
//! answers otherwise is the highest `global_sequence` it assigned — not the
//! first of the latest batch, and not the highest of whichever stream was
//! named last.
//!
//! Run with:
//! `cargo test -p nitinol-persistence --test integration event_store_head::`.

use bytes::Bytes;
use jiff::Timestamp;
use nitinol_persistence::error::AppendError;
use nitinol_persistence::store::{EventStore, InMemoryEventStore, Journal};
use nitinol_persistence::{AppendingEvent, EventType, Family, TypeName};

const A_FACT: EventType = EventType::new(Family::new("event_store.head"), TypeName::new("Fact"));

const FIRST: &str = "event_store.head.first";
const SECOND: &str = "event_store.head.second";

fn a_fact(sequence: u64) -> AppendingEvent {
    AppendingEvent {
        sequence,
        event_type: A_FACT,
        payload: Bytes::from_static(b"payload"),
        occurred_at: Timestamp::now(),
    }
}

/// Where a batch left the numbering off.
fn last_assigned(assigned: &[u64]) -> u64 {
    assigned
        .last()
        .copied()
        .expect("an append of at least one event is assigned at least one global_sequence")
}

/// A single append to one stream and then a batch of three to another, so the
/// last position assigned is neither the first of its batch nor the only
/// position of the stream appended to first. Answers what the batch was
/// assigned.
async fn a_single_append_and_then_a_batch(store: &InMemoryEventStore) -> Vec<u64> {
    store
        .append(FIRST, vec![a_fact(1)])
        .await
        .expect("a fresh stream must take its genesis append");
    store
        .append(SECOND, vec![a_fact(1), a_fact(2), a_fact(3)])
        .await
        .expect("a fresh stream must take a batch of ascending sequences")
        .assigned_sequences
}

/// Given an in-memory store nothing has been appended to, When its head is
/// asked for directly and through a journal over it, Then both answer `None`.
#[tokio::test]
async fn a_store_nothing_was_appended_to_has_no_head() {
    // Given
    let store = InMemoryEventStore::default();
    let journal = Journal::from(store.clone());

    // When
    let direct = store
        .head()
        .await
        .expect("the in-memory store answers its head without a backend to fail");
    let through_the_journal = journal
        .head()
        .await
        .expect("a journal over the in-memory store answers its head");

    // Then
    assert_eq!(
        direct, None,
        "a store that has assigned no global_sequence has no head; a position of zero would \
         read as a journal that ends at an event numbered 0, which no store assigns"
    );
    assert_eq!(
        through_the_journal, None,
        "the journal hands the backend's answer back unchanged, and the backend has no head"
    );
}

/// Given a single append to one stream and a batch to another, When the head is
/// asked for, Then it is the last position the batch was assigned.
#[tokio::test]
async fn the_head_is_the_last_global_sequence_the_store_assigned() {
    // Given
    let store = InMemoryEventStore::default();
    let batch = a_single_append_and_then_a_batch(&store).await;

    // When
    let head = store
        .head()
        .await
        .expect("the in-memory store answers its head without a backend to fail");

    // Then
    assert_eq!(
        head,
        Some(last_assigned(&batch)),
        "the head is the highest global_sequence the store assigned; the batch was assigned \
         {batch:?}, so an answer of its first position, or of the other stream's, falls short \
         of where the journal ends"
    );
}

/// Given a store holding a batch, When an append to another stream is refused,
/// Then the head is where the batch left it.
///
/// The refused append names the stream whose highest position is below the
/// head, so a store answering the highest position of the stream named last
/// is told apart from one answering where the journal ends.
#[tokio::test]
async fn a_refused_append_leaves_the_head_where_it_was() {
    // Given
    let store = InMemoryEventStore::default();
    let batch = a_single_append_and_then_a_batch(&store).await;

    // When
    let refusal = store.append(FIRST, vec![a_fact(1)]).await;
    let head = store
        .head()
        .await
        .expect("the in-memory store answers its head without a backend to fail");

    // Then
    assert!(
        matches!(refusal, Err(AppendError::SequenceConflict(_))),
        "a second append of sequence 1 is a sequence conflict, and the head is only judged \
         across a refusal; got {refusal:?}"
    );
    assert_eq!(
        head,
        Some(last_assigned(&batch)),
        "a refused append assigns no global_sequence, so the head stays at the last position \
         the batch {batch:?} was assigned"
    );
}

/// Given a journal over a store appended to through another handle, When the
/// journal's head is asked for, Then it is the backend's own.
#[tokio::test]
async fn a_journal_answers_with_its_backends_head() {
    // Given
    let backend = InMemoryEventStore::default();
    let journal = Journal::from(backend.clone());
    let batch = a_single_append_and_then_a_batch(&backend).await;

    // When
    let through_the_journal = journal
        .head()
        .await
        .expect("a journal over the in-memory store answers its head");

    // Then
    assert_eq!(
        through_the_journal,
        Some(last_assigned(&batch)),
        "a journal's head is its backend's, unchanged; the backend assigned {batch:?} through a \
         handle that shares its state with the one the journal holds"
    );
}
