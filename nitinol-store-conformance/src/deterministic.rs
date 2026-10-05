use std::collections::HashSet;
use std::sync::Arc;

use bytes::Bytes;
use jiff::Timestamp;
use nitinol_persistence::error::AppendError;
use nitinol_persistence::store::EventStore;
use nitinol_persistence::{AppendOutcome, AppendingEvent, EventType, LoadQuery, MaterializedPath};

use crate::fixture;
use crate::under_test::StoreUnderTest;

/// Put a store through every clause a bad interleaving is not needed for.
///
/// Each clause constructs the situation it judges, so a pass is evidence about
/// the store rather than about how a run happened to be scheduled.  Nothing
/// here spawns anything, which is why no executor is asked for: a clause that
/// needed one would belong in
/// [`verify_under_concurrency`](crate::verify_under_concurrency).
///
/// The clauses run in the order below, and the first one broken panics naming
/// its OCC label.
pub async fn verify_deterministic<S: StoreUnderTest>(store: &S) {
    a_sequence_the_stream_already_holds_is_refused(store).await;
    a_sequence_repeated_within_one_batch_is_refused(store).await;
    a_genesis_collision_leaves_the_first_creation_intact(store).await;
    a_refused_append_consumes_no_global_sequence(store).await;
    an_append_becomes_visible_as_one_commit_unit(store).await;
    a_load_serves_events_in_ascending_global_sequence_order(store).await;
    a_refusal_leaves_every_observable_part_of_the_store_as_it_was(store).await;
    a_prefix_and_a_position_select_the_events_satisfying_both(store).await;
    a_prefix_no_stored_event_is_within_selects_nothing(store).await;
}

/// OCC-1. A `(stream, sequence)` pair the stream already holds is refused, and
/// noticing that is the store's work rather than the caller's.
async fn a_sequence_the_stream_already_holds_is_refused<S: StoreUnderTest>(store: &S) {
    const CLAUSE: &str = "OCC-1";
    const STREAM: &str = "occ.conformance.duplicate";

    let backend = store.bring_up().await;
    committed(CLAUSE, &backend, STREAM, vec![fixture::fact(1, b"first")]).await;

    let refusal = backend
        .append(STREAM, vec![fixture::fact(1, b"again")])
        .await;

    conflicted(CLAUSE, "a second append of sequence 1", refusal);
}

/// OCC-1. The same sequence twice inside one batch is refused as well, and
/// nothing of the batch is stored.
async fn a_sequence_repeated_within_one_batch_is_refused<S: StoreUnderTest>(store: &S) {
    const CLAUSE: &str = "OCC-1";
    const STREAM: &str = "occ.conformance.intra-batch";

    let backend = store.bring_up().await;

    let refusal = backend
        .append(
            STREAM,
            vec![fixture::fact(5, b"first"), fixture::fact(5, b"duplicate")],
        )
        .await;

    conflicted(CLAUSE, "a batch carrying sequence 5 twice", refusal);
    let stored = fixture::observe(&backend, LoadQuery::by_stream(STREAM)).await;
    assert!(
        stored.is_empty(),
        "{CLAUSE} is broken: a batch refused for carrying one sequence twice left {} event(s) \
         in {STREAM:?}, so part of a refused batch was stored",
        stored.len()
    );
}

/// OCC-2. A second append at the genesis sequence conflicts, and the creation
/// already in the stream survives it untouched.
async fn a_genesis_collision_leaves_the_first_creation_intact<S: StoreUnderTest>(store: &S) {
    const CLAUSE: &str = "OCC-2";
    const STREAM: &str = "occ.conformance.genesis";

    let backend = store.bring_up().await;
    let genesis = committed(
        CLAUSE,
        &backend,
        STREAM,
        vec![fixture::fact(1, b"genesis-original")],
    )
    .await;

    let refusal = backend
        .append(STREAM, vec![fixture::fact(1, b"genesis-retry")])
        .await;

    conflicted(CLAUSE, "a second append of the genesis sequence", refusal);
    let stored = fixture::observe(&backend, LoadQuery::by_stream(STREAM)).await;
    assert_eq!(
        stored.len(),
        1,
        "{CLAUSE} is broken: a refused genesis retry left {STREAM:?} holding {} events instead \
         of the one creation it was given",
        stored.len()
    );
    let survivor = &stored[0];
    assert_eq!(
        survivor.payload,
        Bytes::from_static(b"genesis-original"),
        "{CLAUSE} is broken: the creation in {STREAM:?} carries the refused retry's payload, so \
         the retry overwrote the first creation instead of conflicting with it"
    );
    assert_eq!(
        survivor.global_sequence, genesis[0],
        "{CLAUSE} is broken: the first creation was assigned global_sequence {} and is stored \
         under {}, so the refused retry renumbered it",
        genesis[0], survivor.global_sequence
    );
}

/// OCC-3. `global_sequence` is spent only on a committed write, so the append
/// after a refusal continues the numbering without a gap — whichever shape the
/// refused append had.
async fn a_refused_append_consumes_no_global_sequence<S: StoreUnderTest>(store: &S) {
    const CLAUSE: &str = "OCC-3";
    const STREAM: &str = "occ.conformance.numbering";

    let backend = store.bring_up().await;
    let mut last =
        highest(committed(CLAUSE, &backend, STREAM, vec![fixture::fact(1, b"e1")]).await);

    conflicted(
        CLAUSE,
        "a second append of sequence 1",
        backend
            .append(STREAM, vec![fixture::fact(1, b"duplicate")])
            .await,
    );
    last = continues_from(
        CLAUSE,
        "a single sequence the stream already held",
        last,
        committed(CLAUSE, &backend, STREAM, vec![fixture::fact(2, b"e2")]).await,
    );

    conflicted(
        CLAUSE,
        "a batch whose last event repeats sequence 1",
        backend
            .append(
                STREAM,
                vec![fixture::fact(3, b"fresh"), fixture::fact(1, b"duplicate")],
            )
            .await,
    );
    last = continues_from(
        CLAUSE,
        "a duplicate at the tail of a batch",
        last,
        committed(CLAUSE, &backend, STREAM, vec![fixture::fact(3, b"e3")]).await,
    );

    conflicted(
        CLAUSE,
        "a batch carrying sequence 4 twice",
        backend
            .append(
                STREAM,
                vec![fixture::fact(4, b"one"), fixture::fact(4, b"other")],
            )
            .await,
    );
    continues_from(
        CLAUSE,
        "a duplicate within one batch",
        last,
        committed(
            CLAUSE,
            &backend,
            STREAM,
            vec![fixture::fact(4, b"e4"), fixture::fact(5, b"e5")],
        )
        .await,
    );
}

/// OCC-3. One `append(Vec)` becomes visible as one commit unit: a reader sees
/// all of it or none of it, never event N+1 without event N.
async fn an_append_becomes_visible_as_one_commit_unit<S: StoreUnderTest>(store: &S) {
    const CLAUSE: &str = "OCC-3";
    const STREAM: &str = "occ.conformance.commit-unit";
    const NEIGHBOUR: &str = "occ.conformance.commit-unit.neighbour";

    let backend = store.bring_up().await;
    // A stream beside the batch, so a store serving the batch short is told
    // apart from one that answers loads with nothing at all.
    committed(
        CLAUSE,
        &backend,
        NEIGHBOUR,
        vec![fixture::fact(1, b"neighbour")],
    )
    .await;

    let batch = committed(
        CLAUSE,
        &backend,
        STREAM,
        vec![
            fixture::fact(1, b"b1"),
            fixture::fact(2, b"b2"),
            fixture::fact(3, b"b3"),
        ],
    )
    .await;

    let visible: HashSet<u64> = fixture::observe(&backend, LoadQuery::default())
        .await
        .into_iter()
        .map(|event| event.global_sequence)
        .collect();
    let served: Vec<u64> = batch
        .iter()
        .copied()
        .filter(|global_sequence| visible.contains(global_sequence))
        .collect();
    assert_eq!(
        served, batch,
        "{CLAUSE} is broken: one append committed the global_sequences {batch:?} and a load \
         served {served:?} of them; a single append(Vec) becomes visible as one commit unit, so \
         no reader observes part of a batch"
    );
}

/// OCC-3. `global_sequence` increases across the store, and a load serves
/// events in that order whichever stream each one belongs to.
async fn a_load_serves_events_in_ascending_global_sequence_order<S: StoreUnderTest>(store: &S) {
    const CLAUSE: &str = "OCC-3";
    const FIRST: &str = "occ.conformance.order.first";
    const SECOND: &str = "occ.conformance.order.second";

    let backend = store.bring_up().await;
    // The second stream is numbered from 7 rather than from its genesis, so the
    // order read back cannot be the per-stream sequence wearing another name.
    committed(CLAUSE, &backend, FIRST, vec![fixture::fact(1, b"first-1")]).await;
    committed(
        CLAUSE,
        &backend,
        SECOND,
        vec![fixture::fact(7, b"second-1")],
    )
    .await;
    committed(CLAUSE, &backend, FIRST, vec![fixture::fact(2, b"first-2")]).await;

    let served = fixture::observe(&backend, LoadQuery::default()).await;

    let numbering: Vec<u64> = served.iter().map(|event| event.global_sequence).collect();
    let mut ascending = numbering.clone();
    ascending.sort_unstable();
    ascending.dedup();
    assert_eq!(
        numbering, ascending,
        "{CLAUSE} is broken: a load served the global_sequences {numbering:?}, which do not \
         ascend without repeating; global_sequence increases across the store and a load serves \
         events in that order"
    );

    let payloads: Vec<&[u8]> = served.iter().map(|event| event.payload.as_ref()).collect();
    assert_eq!(
        payloads,
        vec![
            b"first-1".as_slice(),
            b"second-1".as_slice(),
            b"first-2".as_slice()
        ],
        "{CLAUSE} is broken: global_sequence order must reproduce the order the appends were \
         made in, across every stream the store holds"
    );
}

/// OCC-4. A sequence conflict is a tripwire: it is answered as it is found,
/// leaving every observable part of the store as it was.
async fn a_refusal_leaves_every_observable_part_of_the_store_as_it_was<S: StoreUnderTest>(
    store: &S,
) {
    const CLAUSE: &str = "OCC-4";
    const STREAM: &str = "occ.conformance.tripwire";
    const NEIGHBOUR: &str = "occ.conformance.tripwire.neighbour";

    let backend = store.bring_up().await;
    committed(CLAUSE, &backend, STREAM, vec![fixture::fact(1, b"t1")]).await;
    committed(CLAUSE, &backend, STREAM, vec![fixture::fact(2, b"t2")]).await;
    committed(CLAUSE, &backend, NEIGHBOUR, vec![fixture::fact(1, b"n1")]).await;

    let before = content(&backend).await;
    assert_eq!(
        before.len(),
        3,
        "{CLAUSE} is broken: the three appends the store took before the refusal must all be \
         there for a comparison across it to say anything, and the store holds {}",
        before.len()
    );

    let refusal = backend
        .append(
            STREAM,
            vec![fixture::fact(3, b"fresh"), fixture::fact(1, b"collision")],
        )
        .await;

    conflicted(
        CLAUSE,
        "a batch mixing a fresh sequence with one the stream already held",
        refusal,
    );
    let after = content(&backend).await;
    assert_eq!(
        after, before,
        "{CLAUSE} is broken: a refused append changed the store's observable content from \
         {before:?} to {after:?}; a sequence conflict is returned as it is found, with no \
         reload and no retry behind it"
    );
}

/// LOAD-1. An event-type prefix and a global position are a conjunction: what
/// a load serves is within the prefix **and** at or above the position.
///
/// A store treating the two as an alternative is caught from this side: the
/// event below the position is within the prefix, so an alternative would serve
/// it.
async fn a_prefix_and_a_position_select_the_events_satisfying_both<S: StoreUnderTest>(store: &S) {
    const CLAUSE: &str = "LOAD-1";
    const FIRST: &str = "occ.conformance.conjunction.first";
    const SECOND: &str = "occ.conformance.conjunction.second";

    let backend = store.bring_up().await;
    // Two streams, so neither condition can be answered by the stream key
    // instead, and the position has events on both sides of it.
    let below = highest(
        committed(
            CLAUSE,
            &backend,
            FIRST,
            vec![fixture::fact(1, b"below-first")],
        )
        .await,
    );
    committed(
        CLAUSE,
        &backend,
        SECOND,
        vec![fixture::fact(1, b"below-second")],
    )
    .await;
    let floor = highest(committed(CLAUSE, &backend, FIRST, vec![fixture::fact(2, b"at")]).await);
    let above =
        highest(committed(CLAUSE, &backend, SECOND, vec![fixture::fact(2, b"above")]).await);

    // The declaration's own path, then a family above it: a prefix is matched
    // on path boundaries, and the position condition holds under either.
    for prefix in [fixture::FACT.to_path(), path("occ.conformance")] {
        let served = fixture::observe(
            &backend,
            LoadQuery {
                event_type_prefix: Some(prefix.clone()),
                from_global_sequence: Some(floor),
                ..LoadQuery::default()
            },
        )
        .await;

        let positions: Vec<u64> = served.iter().map(|event| event.global_sequence).collect();
        assert_eq!(
            positions,
            vec![floor, above],
            "{CLAUSE} is broken: a load naming the prefix {prefix} and the position {floor} \
             served the positions {positions:?}. What is served is the events within the prefix \
             at or above the position, in ascending global order: the event at {below} is within \
             the prefix and below the position, so it is left out, and the position names the \
             first event admitted rather than the first excluded"
        );
    }
}

/// LOAD-1. A prefix no stored event is within serves nothing, whatever the
/// position asked from.
///
/// The other side of the conjunction: every stored event is at or above the
/// position, so a store treating the conditions as an alternative serves them
/// all.
async fn a_prefix_no_stored_event_is_within_selects_nothing<S: StoreUnderTest>(store: &S) {
    const CLAUSE: &str = "LOAD-1";
    const STREAM: &str = "occ.conformance.disjoint";

    let backend = store.bring_up().await;
    let stored = committed(
        CLAUSE,
        &backend,
        STREAM,
        vec![
            fixture::fact(1, b"one"),
            fixture::fact(2, b"two"),
            fixture::fact(3, b"three"),
        ],
    )
    .await;

    // A sibling of the stored declaration, so the two paths share every
    // character of their family and part nowhere but at the last name.
    let absent = path("occ.conformance.Absent");
    for floor in stored {
        let served = fixture::observe(
            &backend,
            LoadQuery {
                event_type_prefix: Some(absent.clone()),
                from_global_sequence: Some(floor),
                ..LoadQuery::default()
            },
        )
        .await;

        assert!(
            served.is_empty(),
            "{CLAUSE} is broken: no stored event is within the prefix {absent}, so no position \
             can admit one — and asked from {floor} the store served {} event(s). The two \
             conditions are a conjunction, not an alternative",
            served.len()
        );
    }
}

// Staging and judging a clause

/// Every observable field of everything the store holds, in the order a load
/// serves it.
///
/// [`LoadedEvent`](nitinol_persistence::LoadedEvent) deliberately does not
/// derive `PartialEq`, so the comparison OCC-4 needs is spelled out here rather
/// than by widening the persistence crate's public API.
#[derive(Debug, PartialEq)]
struct Stored {
    stream_key: String,
    sequence: u64,
    global_sequence: u64,
    event_type: EventType,
    payload: Bytes,
    occurred_at: Timestamp,
}

async fn content(backend: &Arc<dyn EventStore>) -> Vec<Stored> {
    fixture::observe(backend, LoadQuery::default())
        .await
        .into_iter()
        .map(|event| Stored {
            stream_key: event.stream_key,
            sequence: event.sequence,
            global_sequence: event.global_sequence,
            event_type: event.event_type,
            payload: event.payload,
            occurred_at: event.occurred_at,
        })
        .collect()
}

/// The global sequences an append a clause requires to succeed was assigned.
async fn committed(
    clause: &str,
    backend: &Arc<dyn EventStore>,
    key: &str,
    events: Vec<AppendingEvent>,
) -> Vec<u64> {
    let sequences: Vec<u64> = events.iter().map(|event| event.sequence).collect();
    let outcome = backend.append(key, events).await.unwrap_or_else(|refusal| {
        panic!(
            "{clause} is broken: appending the sequences {sequences:?} to {key:?}, none of which \
             that stream holds, was refused with {refusal}"
        )
    });
    assert_eq!(
        outcome.assigned_sequences.len(),
        sequences.len(),
        "{clause} is broken: an append of {} event(s) reported {} assigned global_sequence(s); \
         a committed write is assigned exactly one each",
        sequences.len(),
        outcome.assigned_sequences.len()
    );
    outcome.assigned_sequences
}

/// A refusal a clause requires to be a sequence conflict.
fn conflicted(clause: &str, what: &str, outcome: Result<AppendOutcome, AppendError>) {
    match outcome {
        Err(AppendError::SequenceConflict(_)) => {}
        Err(other) => panic!(
            "{clause} is broken: {what} is a sequence conflict, and the store reported {other}"
        ),
        Ok(taken) => panic!(
            "{clause} is broken: {what} is a sequence conflict, and the store took it, assigning \
             {:?}",
            taken.assigned_sequences
        ),
    }
}

/// A prefix a clause names.
///
/// A literal with no empty segment always parses, so a failure here is the
/// suite failing to run rather than a contract being broken, and it names no
/// clause.
fn path(text: &str) -> MaterializedPath {
    text.parse()
        .expect("a dotted literal with no empty segment is a materialized path")
}

/// Where an append left the numbering off.
fn highest(assigned: Vec<u64>) -> u64 {
    assigned
        .last()
        .copied()
        .expect("an append of at least one event is assigned at least one global_sequence")
}

/// Check that `assigned` picks the numbering up where `last` left it, and
/// return where it leaves off in turn.
fn continues_from(clause: &str, refused: &str, last: u64, assigned: Vec<u64>) -> u64 {
    let expected: Vec<u64> = (1..=assigned.len() as u64)
        .map(|step| last + step)
        .collect();
    assert_eq!(
        assigned, expected,
        "{clause} is broken: the numbering had reached {last} when {refused} was refused, and \
         the next successful append was assigned {assigned:?} where it continues at {expected:?}; \
         a refused append consumes no global_sequence"
    );
    last + assigned.len() as u64
}
