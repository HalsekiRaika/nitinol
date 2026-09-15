//! The OCC clauses as the acceptance criteria state them for this provider.
//!
//! ```gherkin
//! Feature: nitinol-sqlx EventStoreProvider (Postgres)
//!
//!   Scenario: a stale sequence is refused
//!     Given a stream that already holds a sequence
//!     When an append carries that sequence again
//!     Then the append is refused as a sequence conflict naming the stream
//!     And the store holds what it held before
//!
//!   Scenario: events replay in the order they were appended
//!
//!   Scenario: two appends racing for one sequence leave one winner
//!     And only the winner's event is stored
//!
//!   Scenario: a refused append consumes no numbering, whichever stream
//!             appends next
//! ```
//!
//! The conformance suite reaches the same labels from the store's side. What is
//! fixed here is what it leaves open: the stream key a conflict carries, two
//! appends actually racing on one connection pool, the numbering a *different*
//! stream picks up after a refusal, and the empty batch.
//!
//! Running any of this needs a database. Where none can be reached the clause
//! judges nothing, which is not evidence that it holds.

#[path = "common/fixture.rs"]
mod fixture;
#[path = "common/harness.rs"]
mod harness;

use std::sync::Arc;

use fixture::{
    committed, conflicted, content, continues_from, highest, observe, payloads, recorded,
};
use nitinol_persistence::error::AppendError;
use nitinol_persistence::LoadQuery;

#[tokio::test]
async fn a_sequence_the_stream_already_holds_is_refused_by_the_stream_key_and_changes_nothing() {
    const STREAM: &str = "sqlx.contract.conflict";

    let Some(backend) = harness::postgres_backend(&[fixture::RECORDED]).await else {
        return;
    };
    let store = backend.bring_up_store().await.store;
    committed(
        &store,
        STREAM,
        vec![recorded(1, b"first"), recorded(2, b"second")],
    )
    .await;
    let before = content(&store).await;

    let refusal = store.append(STREAM, vec![recorded(2, b"stale")]).await;

    match refusal {
        Err(AppendError::SequenceConflict(key)) => assert_eq!(
            key, STREAM,
            "a sequence conflict carries the key of the stream it was raised for"
        ),
        Err(other) => panic!(
            "appending a sequence {STREAM:?} already holds is a sequence conflict, and the store \
             reported {other}"
        ),
        Ok(outcome) => panic!(
            "appending a sequence {STREAM:?} already holds is a sequence conflict, and the store \
             took it, assigning {:?}",
            outcome.assigned_sequences
        ),
    }
    let after = content(&store).await;
    assert_eq!(
        after, before,
        "a refused append changed the store's observable content from {before:?} to {after:?}"
    );
}

#[tokio::test]
async fn events_appended_to_one_stream_replay_in_the_order_they_were_appended() {
    const STREAM: &str = "sqlx.contract.replay";

    let Some(backend) = harness::postgres_backend(&[fixture::RECORDED]).await else {
        return;
    };
    let store = backend.bring_up_store().await.store;
    committed(&store, STREAM, vec![recorded(1, b"e1")]).await;
    committed(&store, STREAM, vec![recorded(2, b"e2"), recorded(3, b"e3")]).await;
    committed(&store, STREAM, vec![recorded(4, b"e4")]).await;

    let served = observe(&store, LoadQuery::by_stream(STREAM)).await;

    assert_eq!(
        payloads(&served),
        vec![
            b"e1".as_slice(),
            b"e2".as_slice(),
            b"e3".as_slice(),
            b"e4".as_slice()
        ],
        "a stream replays in the order it was appended in"
    );
    let sequences: Vec<u64> = served.iter().map(|event| event.sequence).collect();
    assert_eq!(
        sequences,
        vec![1, 2, 3, 4],
        "every event replays under the sequence it was appended with"
    );
    let numbering: Vec<u64> = served.iter().map(|event| event.global_sequence).collect();
    let mut ascending = numbering.clone();
    ascending.sort_unstable();
    ascending.dedup();
    assert_eq!(
        numbering, ascending,
        "a replay is served in ascending global_sequence order, and {numbering:?} does not ascend \
         without repeating"
    );
    let keys: Vec<&str> = served
        .iter()
        .map(|event| event.stream_key.as_str())
        .collect();
    assert_eq!(
        keys,
        vec![STREAM; 4],
        "every event replays under the stream key it was appended to"
    );
}

#[tokio::test]
async fn an_append_reports_the_highest_sequence_of_its_batch_as_the_stream_version() {
    const STREAM: &str = "sqlx.contract.version";

    let Some(backend) = harness::postgres_backend(&[fixture::RECORDED]).await else {
        return;
    };
    let store = backend.bring_up_store().await.store;

    let genesis = committed(&store, STREAM, vec![recorded(1, b"e1")]).await;
    assert_eq!(
        genesis.stream_version, 1,
        "an append of sequence 1 leaves the stream at version 1"
    );

    let batch = committed(&store, STREAM, vec![recorded(2, b"e2"), recorded(3, b"e3")]).await;
    assert_eq!(
        batch.stream_version, 3,
        "an append reports the highest sequence of the batch it took"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_appends_racing_for_one_sequence_leave_exactly_one_winner() {
    const STREAM: &str = "sqlx.contract.race";

    let Some(backend) = harness::postgres_backend(&[fixture::RECORDED]).await else {
        return;
    };
    let store = backend.bring_up_store().await.store;

    let one = tokio::spawn({
        let store = Arc::clone(&store);
        async move { store.append(STREAM, vec![recorded(1, b"one")]).await }
    });
    let other = tokio::spawn({
        let store = Arc::clone(&store);
        async move { store.append(STREAM, vec![recorded(1, b"other")]).await }
    });
    let one = one.await.expect("a racing append must run to completion");
    let other = other.await.expect("a racing append must run to completion");

    let winner: &[u8] = match (&one, &other) {
        (Ok(_), Err(AppendError::SequenceConflict(key))) => {
            assert_eq!(
                key, STREAM,
                "the losing append is refused for the stream it raced on"
            );
            b"one"
        }
        (Err(AppendError::SequenceConflict(key)), Ok(_)) => {
            assert_eq!(
                key, STREAM,
                "the losing append is refused for the stream it raced on"
            );
            b"other"
        }
        (one, other) => panic!(
            "two appends of sequence 1 to {STREAM:?} leave one winner and one sequence conflict, \
             and the store answered {one:?} and {other:?}"
        ),
    };

    let stored = observe(&store, LoadQuery::by_stream(STREAM)).await;
    assert_eq!(
        payloads(&stored),
        vec![winner],
        "only the append that won the race is stored"
    );
}

#[tokio::test]
async fn a_refused_append_consumes_no_numbering_for_the_stream_that_appends_next() {
    const REFUSED: &str = "sqlx.contract.numbering.refused";

    let Some(backend) = harness::postgres_backend(&[fixture::RECORDED]).await else {
        return;
    };
    let store = backend.bring_up_store().await.store;
    let mut last = highest(committed(&store, REFUSED, vec![recorded(1, b"e1")]).await);

    conflicted(
        &store,
        REFUSED,
        vec![recorded(1, b"duplicate")],
        "a single sequence the stream already held",
    )
    .await;
    last = continues_from(
        last,
        "a single sequence the stream already held",
        committed(
            &store,
            "sqlx.contract.numbering.after-single",
            vec![recorded(1, b"n1")],
        )
        .await,
    );

    conflicted(
        &store,
        REFUSED,
        vec![recorded(2, b"fresh"), recorded(1, b"duplicate")],
        "a duplicate at the tail of a batch",
    )
    .await;
    last = continues_from(
        last,
        "a duplicate at the tail of a batch",
        committed(
            &store,
            "sqlx.contract.numbering.after-tail",
            vec![recorded(1, b"n2")],
        )
        .await,
    );

    conflicted(
        &store,
        REFUSED,
        vec![recorded(3, b"one"), recorded(3, b"other")],
        "a duplicate within one batch",
    )
    .await;
    continues_from(
        last,
        "a duplicate within one batch",
        committed(
            &store,
            "sqlx.contract.numbering.after-intra-batch",
            vec![recorded(1, b"n3"), recorded(2, b"n4")],
        )
        .await,
    );
}

#[tokio::test]
async fn an_empty_batch_is_a_no_op_that_reports_the_stream_version_and_consumes_no_numbering() {
    const STREAM: &str = "sqlx.contract.empty";
    const NEXT: &str = "sqlx.contract.empty.next";

    let Some(backend) = harness::postgres_backend(&[fixture::RECORDED]).await else {
        return;
    };
    let store = backend.bring_up_store().await.store;

    let untouched = committed(&store, STREAM, vec![]).await;
    assert!(
        untouched.assigned_sequences.is_empty(),
        "an empty batch is assigned no global_sequence, and reported {:?}",
        untouched.assigned_sequences
    );
    assert_eq!(
        untouched.stream_version, 0,
        "a stream holding nothing is at version 0"
    );

    let last =
        highest(committed(&store, STREAM, vec![recorded(1, b"e1"), recorded(2, b"e2")]).await);

    let after = committed(&store, STREAM, vec![]).await;
    assert!(
        after.assigned_sequences.is_empty(),
        "an empty batch is assigned no global_sequence, and reported {:?}",
        after.assigned_sequences
    );
    assert_eq!(
        after.stream_version, 2,
        "an empty batch reports the sequence the stream had reached"
    );

    continues_from(
        last,
        "an empty batch",
        committed(&store, NEXT, vec![recorded(1, b"n1")]).await,
    );
}
