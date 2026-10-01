//! The fan-out pattern under at-least-once delivery.
//!
//! One decision is recorded as one fact event on one stream; the saga manager
//! turns that fact into 32 payslip issues; and a second incarnation over the same
//! durable state re-dispatches every issue whose settlement the first one never
//! recorded.  The pattern's claim is that those repeats change nothing observable
//! in the payslip streams — the idempotence lives in the payslips, not in a
//! short-circuit inside the saga.
//!
//! The fact event itself is delivered a second time as well, because the manager's
//! cursor is not durable, and it is *not* fanned out again: its position is on the
//! saga's own stream, committed by the append that carried the decision, so the
//! framework drops it before `Saga::handle`.
//!
//! The store-side half of that claim — a conflict on a stream's genesis sequence
//! means "already created", and the first write stays intact — is stated and
//! fixed by `nitinol-persistence`.  What is fixed here is the caller-side half
//! the `EventStore::append` contract leaves to convention: a redelivered issue
//! writes nothing new and destroys nothing old.
//!
//! Every stream these tests observe — the payroll run's, the 32 payslips' and
//! the saga's own — lives in the one store the incarnation was given, which is
//! what the assertions below read from.

#[path = "common/helpers.rs"]
mod common;
use common::{
    decisions, drain_payslips, employee_ids, fingerprint, fingerprints, issued, load_stream,
    payslip_keys, spawn_manager, spawn_world, wait_for_issued, wait_for_records,
    InterruptedFanOutStore, DECISION_BLOCK_LEN, EMPLOYEE_COUNT,
};

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use nitinol_eventsource::Event;
use nitinol_persistence::error::AppendError;
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::{AggregateId, AppendingEvent};

use saga_fanout_pattern::payroll_run::{ApprovePayrollRun, PayrollRunApproved};
use saga_fanout_pattern::payslip::{Payslip, PayslipIssued};
use saga_fanout_pattern::saga::{FanOutSaga, FanOutStarted};

/// Budget for a 32-wide fan-out to reach the store.  The manager polls its
/// upstream every 250ms and each tell is dispatched through the outbox, so this
/// is generous rather than tight — a deadline, not a synchronisation device.
const FANOUT_TIMEOUT: Duration = Duration::from_secs(20);

/// The one store an incarnation is wired with.
///
/// `EventStore` is stream-keyed, so a single instance holds the run's stream,
/// the payslip streams and the saga's journal side by side; splitting them
/// across instances is a deployment choice, not something the pattern needs.
fn single_store() -> Arc<dyn EventStore> {
    Arc::new(InMemoryEventStore::default())
}

/// One decision, one append, 32 payslips.
///
/// This pins the shape the rest of the pattern rests on: the decision is a
/// single record on the payroll run's stream (not a per-employee write, and not
/// a write spanning several streams), and the saga's own stream shows the
/// decision and all 32 tell markers committed together.
#[tokio::test]
async fn one_fact_event_records_the_approval_and_fans_out_to_thirty_two_payslips() {
    let store = single_store();
    let run_id = AggregateId::new("payroll-run-single-append");
    let employees = employee_ids();
    let payslips = payslip_keys(run_id.as_str(), &employees);

    let world = spawn_world(&run_id, Arc::clone(&store)).await;

    world
        .payroll_run
        .ask(ApprovePayrollRun {
            payroll_run: run_id.as_str().to_owned(),
            employee_ids: employees.clone(),
        })
        .await
        .expect("ask(ApprovePayrollRun) must succeed");

    wait_for_issued(&store, &payslips, EMPLOYEE_COUNT, FANOUT_TIMEOUT).await;

    let fact_stream = load_stream(&store, run_id.as_str()).await;
    assert_eq!(
        fact_stream.len(),
        1,
        "the decision must reach the payroll run's stream as exactly one record; \
         more than one means the decision was split across appends"
    );
    assert_eq!(
        fact_stream[0].sequence, 1,
        "the fact event must occupy the payroll run stream's genesis sequence"
    );
    assert_eq!(
        fact_stream[0].event_type,
        PayrollRunApproved::EVENT_TYPE,
        "the single record on the payroll run's stream must be the fact event"
    );

    for (key, stream) in payslips.iter().zip(fingerprints(&store, &payslips).await) {
        assert_eq!(
            stream.len(),
            1,
            "payslip {key} must hold exactly one issue record"
        );
        assert_eq!(
            stream[0].0, 1,
            "payslip {key}'s issue must occupy its genesis sequence"
        );
    }

    // The saga records its decision and every tell it derived from it in one
    // commit unit.  `append(Vec)` stamps one `occurred_at` over that whole
    // commit unit, so a decision split into per-tell appends would show more
    // than one timestamp across the leading block.
    let saga_key = FanOutSaga::instance_id(run_id.as_str());
    let saga_stream = load_stream(&store, saga_key.as_str()).await;
    assert!(
        saga_stream.len() >= DECISION_BLOCK_LEN,
        "the saga stream must hold the decision, its {EMPLOYEE_COUNT} tell markers \
         and the position of the fact it was derived from (had {} records)",
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
         records are the framework's — {EMPLOYEE_COUNT} tell markers and the \
         consumed position of the fact event"
    );
    assert!(
        block.iter().all(|e| e.occurred_at == block[0].occurred_at),
        "every record in the decision block must carry the timestamp of the one \
         append that committed it"
    );
}

/// Every issue delivered a second time must leave the payslips untouched.
///
/// The first incarnation writes through a store that refuses to record anything
/// beyond the append that opened the saga's journal, so all 32 payslips are issued
/// while every marker for them stays pending — the durable state a process that
/// dies mid-fan-out leaves behind.  The manager is then stopped and spawned again
/// over the same store, and it is those pending markers, not a second decision,
/// that deliver all 32 issues again.
///
/// The payslip processes stay resident across the restart, so the repeats are
/// absorbed in their plain form — no aggregate replay involved.
#[tokio::test]
async fn a_redispatched_issue_writes_no_second_payslip() {
    let store = single_store();
    let run_id = AggregateId::new("payroll-run-redelivery");
    let employees = employee_ids();
    let payslips = payslip_keys(run_id.as_str(), &employees);
    let saga_key = FanOutSaga::instance_id(run_id.as_str());

    // Every payslip is reachable here: what goes unrecorded is only the settling
    // of the dispatches.
    let interrupted = InterruptedFanOutStore::new(
        Arc::clone(&store),
        saga_key.as_str().to_owned(),
        HashSet::new(),
    );

    let world = spawn_world(&run_id, Arc::clone(&interrupted) as Arc<dyn EventStore>).await;

    world
        .payroll_run
        .ask(ApprovePayrollRun {
            payroll_run: run_id.as_str().to_owned(),
            employee_ids: employees.clone(),
        })
        .await
        .expect("ask(ApprovePayrollRun) must succeed");

    wait_for_issued(&store, &payslips, EMPLOYEE_COUNT, FANOUT_TIMEOUT).await;
    let before = fingerprints(&store, &payslips).await;

    let unsettled = wait_for_records(
        &store,
        saga_key.as_str(),
        DECISION_BLOCK_LEN,
        FANOUT_TIMEOUT,
    )
    .await;
    assert_eq!(
        unsettled.len(),
        DECISION_BLOCK_LEN,
        "the interrupted incarnation must leave every dispatch unsettled: the decision \
         block alone, with no terminal marker after it (journal holds {} records)",
        unsettled.len()
    );

    // Stopping the manager cascade-stops its poller and its saga instances;
    // dropping the handle alone would leave them running.
    world
        .manager
        .stop()
        .await
        .expect("stopping the manager must succeed");

    // The interrupted incarnation is gone, so the store the next one writes to
    // records everything again.
    interrupted.incarnation_ended();

    let _second_manager = spawn_manager(&world.system, &run_id).await;

    // One terminal marker per re-dispatched issue is what says all 32 were
    // dispatched again, and each is written after its command was handed to the
    // payslip — so this is the observable point the assertions below need, not a
    // guessed settle window.
    wait_for_records(
        &store,
        saga_key.as_str(),
        DECISION_BLOCK_LEN + EMPLOYEE_COUNT,
        FANOUT_TIMEOUT,
    )
    .await;

    let states = drain_payslips(&world.system, &payslips).await;
    assert_eq!(
        states,
        vec![true; EMPLOYEE_COUNT],
        "every payslip must report itself issued after the re-dispatch"
    );

    let after = fingerprints(&store, &payslips).await;
    assert_eq!(
        after, before,
        "a re-dispatched issue must leave every payslip stream byte-identical: \
         same record count, same stream sequences, same global sequences, same \
         payloads"
    );
    for (key, stream) in payslips.iter().zip(&after) {
        assert_eq!(
            stream.len(),
            1,
            "payslip {key} must still hold exactly one issue record after the re-dispatch"
        );
    }

    let saga_stream = load_stream(&store, saga_key.as_str()).await;
    assert_eq!(
        saga_stream.len(),
        DECISION_BLOCK_LEN + EMPLOYEE_COUNT,
        "the 32 re-dispatches must settle as one marker each: a dispatch that could \
         not be delivered settles as a failure with a dead letter beside it (journal \
         holds {} records)",
        saga_stream.len()
    );
    assert_eq!(
        decisions(&saga_stream).len(),
        1,
        "the redelivered fact event must not be fanned out a second time: its position \
         is on the saga's own stream, so the framework drops it before `handle`"
    );

    assert_eq!(
        load_stream(&store, run_id.as_str()).await.len(),
        1,
        "the re-dispatch must not add records to the stream that owns the decision"
    );
    assert_eq!(
        issued(&store, &payslips).await.len(),
        EMPLOYEE_COUNT,
        "the re-dispatch must not issue payslips beyond the ones the fact event named"
    );
}

/// The caller-side reading of a genesis conflict.
///
/// An issue redelivered all the way down to the store conflicts on the
/// payslip's genesis sequence.  For an issue-only fan-out that conflict is the
/// success answer — "this payslip already exists" — and the example depends on
/// the first write surviving it untouched, which is what makes the fan-out need
/// no compensation.
#[tokio::test]
async fn duplicate_genesis_append_conflicts_and_preserves_the_first_write() {
    let store = single_store();
    let run_id = AggregateId::new("payroll-run-genesis-conflict");
    let employees = employee_ids();
    let payslips = payslip_keys(run_id.as_str(), &employees);

    let world = spawn_world(&run_id, Arc::clone(&store)).await;

    world
        .payroll_run
        .ask(ApprovePayrollRun {
            payroll_run: run_id.as_str().to_owned(),
            employee_ids: employees.clone(),
        })
        .await
        .expect("ask(ApprovePayrollRun) must succeed");

    wait_for_issued(&store, &payslips, EMPLOYEE_COUNT, FANOUT_TIMEOUT).await;

    let key = &payslips[0];
    let original = load_stream(&store, key).await;
    assert_eq!(original.len(), 1, "payslip {key} must hold one record");
    let genesis = &original[0];
    assert_eq!(
        genesis.sequence, 1,
        "a payslip's issue is its genesis record, so the genesis-conflict reading applies to it"
    );
    assert_eq!(
        genesis.event_type,
        PayslipIssued::EVENT_TYPE,
        "a payslip's genesis record must be its issue event"
    );

    let before = fingerprint(&store, key).await;

    // Replay the identical issue write the fan-out already committed.
    let outcome = store
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
            "the conflict must name the payslip stream it was raised for"
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
        fingerprint(&store, key).await,
        before,
        "the conflicting append must leave the first write intact — same record \
         count, same sequences, same payload"
    );
}

/// Where the idempotence stops.
///
/// "Already issued" is the right answer to a redelivered issue and the wrong
/// answer to next month's payroll run, and the only thing separating the two is
/// the payslip's stream key: an employee outlives any one run, so a key derived
/// from the employee alone would make every later run's payslip conflict with
/// the first one's and never be issued.  The tests above cannot see that — they
/// approve one run — so the boundary is fixed here, on the derivation that owns
/// it.
#[test]
fn payslips_of_two_payroll_runs_covering_one_employee_are_separate_streams() {
    let employee = &employee_ids()[0];

    assert_ne!(
        Payslip::stream_key("payroll-run-april", employee),
        Payslip::stream_key("payroll-run-may", employee),
        "two payroll runs covering the same employee must address different \
         payslip streams; sharing one would make the later run's issue collide \
         with the earlier run's genesis record and be answered as \
         'already issued'"
    );
}
