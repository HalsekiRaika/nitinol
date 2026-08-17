//! The fan-out pattern under at-least-once redelivery.
//!
//! One decision is recorded as one fact event on one stream; the saga manager
//! turns that fact into 32 child creations; and because the manager's cursor is
//! not durable, a manager spawned a second time over the same batch stream
//! delivers the very same fact event again.  The pattern's claim is that the
//! second delivery changes nothing observable in the child streams — the
//! idempotence lives in the children, not in a short-circuit inside the saga.
//!
//! The store-side half of that claim (`OCC-2`: a conflict on a stream's genesis
//! sequence means "already created", and the first write stays intact) is fixed
//! by `nitinol-persistence`.  What is fixed here is the caller-side half the
//! `EventStore::append` contract leaves to convention: a redelivered creation
//! writes nothing new and destroys nothing old.

#[path = "common/helpers.rs"]
mod common;
use common::{
    created, drain_children, fingerprint, fingerprints, item_ids, load_stream, spawn_manager,
    spawn_world, wait_for_created, wait_for_decisions, DECISION_BLOCK_LEN, ITEM_COUNT,
};

use std::sync::Arc;
use std::time::Duration;

use nitinol_eventsource::Event;
use nitinol_persistence::error::AppendError;
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::{AggregateId, AppendingEvent};

use saga_fanout_pattern::batch::{BatchDecomposed, DecomposeBatch};
use saga_fanout_pattern::item::ItemCreated;
use saga_fanout_pattern::saga::{FanOutSaga, FanOutStarted};

/// Budget for a 32-wide fan-out to reach the item store.  The manager polls its
/// upstream every 250ms and each tell is dispatched through the outbox, so this
/// is generous rather than tight — a deadline, not a synchronisation device.
const FANOUT_TIMEOUT: Duration = Duration::from_secs(20);

/// Settle window used only in front of a negative assertion.
///
/// The saga appends its tell markers and *then* dispatches them, so the moment
/// the second decision becomes visible in the saga stream the redelivered
/// commands may still be in flight.  There is no earlier observable point, so
/// the redelivery is given a bounded window to land before the "nothing
/// changed" assertions run.  Mirrors the settle used by
/// `nitinol-saga/tests/saga_manager_lazy_spawn.rs`.
const REDELIVERY_SETTLE: Duration = Duration::from_millis(750);

fn stores() -> (
    Arc<dyn EventStore>,
    Arc<dyn EventStore>,
    Arc<dyn EventStore>,
) {
    (
        Arc::new(InMemoryEventStore::default()),
        Arc::new(InMemoryEventStore::default()),
        Arc::new(InMemoryEventStore::default()),
    )
}

/// One decision, one append, 32 children.
///
/// This pins the shape the rest of the pattern rests on: the decision is a
/// single record on the batch stream (not a per-child write, and not a write
/// spanning several streams), and the saga's own stream shows the decision and
/// all 32 tell markers committed together.
#[tokio::test]
async fn one_fact_event_records_the_decision_and_fans_out_to_thirty_two_children() {
    let (batch_store, item_store, saga_store) = stores();
    let batch_id = AggregateId::new("fanout-single-append");
    let ids = item_ids(batch_id.as_str());

    let world = spawn_world(
        &batch_id,
        Arc::clone(&batch_store),
        Arc::clone(&item_store),
        Arc::clone(&saga_store),
    )
    .await;

    world
        .batch
        .ask(DecomposeBatch { items: ids.clone() })
        .await
        .expect("ask(DecomposeBatch) must succeed");

    wait_for_created(&item_store, &ids, ITEM_COUNT, FANOUT_TIMEOUT).await;

    let fact_stream = load_stream(&batch_store, batch_id.as_str()).await;
    assert_eq!(
        fact_stream.len(),
        1,
        "the decision must reach the batch stream as exactly one record; \
         more than one means the decision was split across appends"
    );
    assert_eq!(
        fact_stream[0].sequence, 1,
        "the fact event must occupy the batch stream's genesis sequence"
    );
    assert_eq!(
        fact_stream[0].event_type,
        BatchDecomposed::EVENT_TYPE,
        "the single record on the batch stream must be the fact event"
    );

    for (key, stream) in ids.iter().zip(fingerprints(&item_store, &ids).await) {
        assert_eq!(
            stream.len(),
            1,
            "child {key} must hold exactly one creation record"
        );
        assert_eq!(
            stream[0].0, 1,
            "child {key}'s creation must occupy its genesis sequence"
        );
    }

    // The saga records its decision and every tell it derived from it in one
    // commit unit.  `append(Vec)` stamps one `occurred_at` over the whole
    // batch, so a decision split into per-tell appends would show more than one
    // timestamp across the leading block.
    let saga_key = FanOutSaga::instance_id(batch_id.as_str());
    let saga_stream = load_stream(&saga_store, saga_key.as_str()).await;
    assert!(
        saga_stream.len() >= DECISION_BLOCK_LEN,
        "the saga stream must hold the decision and its {ITEM_COUNT} tell markers \
         (had {} records)",
        saga_stream.len()
    );
    let block = &saga_stream[..DECISION_BLOCK_LEN];
    assert_eq!(
        block.iter().map(|e| e.sequence).collect::<Vec<u64>>(),
        (1..=DECISION_BLOCK_LEN as u64).collect::<Vec<u64>>(),
        "the decision block must occupy contiguous sequences from the genesis \
         sequence — a gap or an interleaved record means the decision and its \
         tells did not share one append"
    );
    assert_eq!(
        block[0].event_type,
        FanOutStarted::EVENT_TYPE,
        "the saga's decision event must lead its own stream"
    );
    assert_eq!(
        block
            .iter()
            .filter(|e| e.event_type == FanOutStarted::EVENT_TYPE)
            .count(),
        1,
        "the decision block must carry exactly one domain event; the other \
         {ITEM_COUNT} records are the framework's tell markers"
    );
    assert!(
        block.iter().all(|e| e.occurred_at == block[0].occurred_at),
        "every record in the decision block must carry the timestamp of the one \
         append that committed it"
    );
}

/// The same fact event delivered twice must leave the children untouched.
///
/// The manager is stopped and spawned again over the same batch store; its
/// cursor is in memory, so the second incarnation replays the fact event from
/// the start.  The child processes stay resident across the restart, so this is
/// redelivery in its plain form — no aggregate replay involved.
#[tokio::test]
async fn redelivered_fact_event_writes_no_second_creation() {
    let (batch_store, item_store, saga_store) = stores();
    let batch_id = AggregateId::new("fanout-redelivery");
    let ids = item_ids(batch_id.as_str());

    let world = spawn_world(
        &batch_id,
        Arc::clone(&batch_store),
        Arc::clone(&item_store),
        Arc::clone(&saga_store),
    )
    .await;

    world
        .batch
        .ask(DecomposeBatch { items: ids.clone() })
        .await
        .expect("ask(DecomposeBatch) must succeed");

    wait_for_created(&item_store, &ids, ITEM_COUNT, FANOUT_TIMEOUT).await;
    let before = fingerprints(&item_store, &ids).await;

    // Stopping the manager cascade-stops its poller and its saga instances;
    // dropping the handle alone would leave them running.
    world
        .manager
        .stop()
        .await
        .expect("stopping the manager must succeed");

    let _second_manager = spawn_manager(
        &world.system,
        &world.registry,
        &batch_id,
        Arc::clone(&batch_store),
        Arc::clone(&saga_store),
    )
    .await;

    let saga_key = FanOutSaga::instance_id(batch_id.as_str());
    wait_for_decisions(&saga_store, saga_key.as_str(), 2, FANOUT_TIMEOUT).await;
    tokio::time::sleep(REDELIVERY_SETTLE).await;

    let states = drain_children(&world.registry, &ids).await;
    assert_eq!(
        states,
        vec![true; ITEM_COUNT],
        "every child must report itself created after the redelivery"
    );

    let after = fingerprints(&item_store, &ids).await;
    assert_eq!(
        after, before,
        "a redelivered fact event must leave every child stream byte-identical: \
         same record count, same stream sequences, same global sequences, same \
         payloads"
    );
    for (key, stream) in ids.iter().zip(&after) {
        assert_eq!(
            stream.len(),
            1,
            "child {key} must still hold exactly one creation record after redelivery"
        );
    }

    assert_eq!(
        load_stream(&batch_store, batch_id.as_str()).await.len(),
        1,
        "redelivery must not add records to the stream that owns the decision"
    );
    assert_eq!(
        created(&item_store, &ids).await.len(),
        ITEM_COUNT,
        "redelivery must not create children beyond the ones the fact event named"
    );
}

/// The caller-side reading of `OCC-2`.
///
/// A creation redelivered all the way down to the store conflicts on the
/// child's genesis sequence.  For a creation-only fan-out that conflict is the
/// success answer — "this child already exists" — and the example depends on
/// the first write surviving it untouched, which is what makes the fan-out
/// need no compensation.
#[tokio::test]
async fn duplicate_genesis_append_conflicts_and_preserves_the_first_write() {
    let (batch_store, item_store, saga_store) = stores();
    let batch_id = AggregateId::new("fanout-genesis-conflict");
    let ids = item_ids(batch_id.as_str());

    let world = spawn_world(
        &batch_id,
        Arc::clone(&batch_store),
        Arc::clone(&item_store),
        Arc::clone(&saga_store),
    )
    .await;

    world
        .batch
        .ask(DecomposeBatch { items: ids.clone() })
        .await
        .expect("ask(DecomposeBatch) must succeed");

    wait_for_created(&item_store, &ids, ITEM_COUNT, FANOUT_TIMEOUT).await;

    let key = &ids[0];
    let original = load_stream(&item_store, key).await;
    assert_eq!(original.len(), 1, "child {key} must hold one record");
    let genesis = &original[0];
    assert_eq!(
        genesis.sequence, 1,
        "a child's creation is its genesis record, so OCC-2 applies to it"
    );
    assert_eq!(
        genesis.event_type,
        ItemCreated::EVENT_TYPE,
        "a child's genesis record must be its creation event"
    );

    let before = fingerprint(&item_store, key).await;

    // Replay the identical creation write the fan-out already committed.
    let outcome = item_store
        .append(
            key,
            vec![AppendingEvent {
                sequence: genesis.sequence,
                event_type: genesis.event_type,
                payload: genesis.payload.clone(),
                occurred_at: genesis.occurred_at,
            }],
        )
        .await;

    match outcome {
        Err(AppendError::SequenceConflict(stream)) => assert_eq!(
            &stream, key,
            "the conflict must name the child stream it was raised for"
        ),
        Err(other) => {
            panic!("a repeated genesis append must fail as SequenceConflict, not as {other:?}")
        }
        Ok(committed) => panic!(
            "a repeated genesis append must not commit (assigned {:?})",
            committed.assigned_sequences
        ),
    }

    assert_eq!(
        fingerprint(&item_store, key).await,
        before,
        "the conflicting append must leave the first write intact — same record \
         count, same sequences, same payload"
    );
}
