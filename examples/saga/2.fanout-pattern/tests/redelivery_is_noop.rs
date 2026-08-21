//! The fan-out pattern under at-least-once redelivery.
//!
//! One decision is recorded as one fact event on one stream; the saga manager
//! turns that fact into 32 payslip issues; and because the manager's cursor is
//! not durable, a manager spawned a second time over the same run stream
//! delivers the very same fact event again.  The pattern's claim is that the
//! second delivery changes nothing observable in the payslip streams — the
//! idempotence lives in the payslips, not in a short-circuit inside the saga.
//!
//! The store-side half of that claim (`OCC-2`: a conflict on a stream's genesis
//! sequence means "already created", and the first write stays intact) is fixed
//! by `nitinol-persistence`.  What is fixed here is the caller-side half the
//! `EventStore::append` contract leaves to convention: a redelivered issue
//! writes nothing new and destroys nothing old.
//!
//! Every stream these tests observe — the payroll run's, the 32 payslips' and
//! the saga's own — lives in the one store the incarnation was given, which is
//! what the assertions below read from.

#[path = "common/helpers.rs"]
mod common;
use common::{
    drain_payslips, employee_ids, fingerprint, fingerprints, issued, load_stream, payslip_keys,
    spawn_manager, spawn_world, wait_for_decisions, wait_for_issued, DECISION_BLOCK_LEN,
    EMPLOYEE_COUNT,
};

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

/// Settle window used only in front of a negative assertion.
///
/// The saga appends its tell markers and *then* dispatches them, so the moment
/// the second decision becomes visible in the saga stream the redelivered
/// commands may still be in flight.  There is no earlier observable point, so
/// the redelivery is given a bounded window to land before the "nothing
/// changed" assertions run.  Mirrors the settle used by
/// `nitinol-saga/tests/saga_manager_lazy_spawn.rs`.
const REDELIVERY_SETTLE: Duration = Duration::from_millis(750);

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
        "the saga stream must hold the decision and its {EMPLOYEE_COUNT} tell markers \
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
         {EMPLOYEE_COUNT} records are the framework's tell markers"
    );
    assert!(
        block.iter().all(|e| e.occurred_at == block[0].occurred_at),
        "every record in the decision block must carry the timestamp of the one \
         append that committed it"
    );
}

/// The same fact event delivered twice must leave the payslips untouched.
///
/// The manager is stopped and spawned again over the same store; its cursor is
/// in memory, so the second incarnation replays the fact event from the start.
/// The payslip processes stay resident across the restart, so this is
/// redelivery in its plain form — no aggregate replay involved.
#[tokio::test]
async fn redelivered_fact_event_writes_no_second_payslip() {
    let store = single_store();
    let run_id = AggregateId::new("payroll-run-redelivery");
    let employees = employee_ids();
    let payslips = payslip_keys(run_id.as_str(), &employees);

    let world = spawn_world(&run_id, Arc::clone(&store)).await;

    world
        .payroll_run
        .ask(ApprovePayrollRun {
            employee_ids: employees.clone(),
        })
        .await
        .expect("ask(ApprovePayrollRun) must succeed");

    wait_for_issued(&store, &payslips, EMPLOYEE_COUNT, FANOUT_TIMEOUT).await;
    let before = fingerprints(&store, &payslips).await;

    // Stopping the manager cascade-stops its poller and its saga instances;
    // dropping the handle alone would leave them running.
    world
        .manager
        .stop()
        .await
        .expect("stopping the manager must succeed");

    let _second_manager = spawn_manager(&world.system, &run_id).await;

    let saga_key = FanOutSaga::instance_id(run_id.as_str());
    wait_for_decisions(&store, saga_key.as_str(), 2, FANOUT_TIMEOUT).await;
    tokio::time::sleep(REDELIVERY_SETTLE).await;

    let states = drain_payslips(&world.system, &payslips).await;
    assert_eq!(
        states,
        vec![true; EMPLOYEE_COUNT],
        "every payslip must report itself issued after the redelivery"
    );

    let after = fingerprints(&store, &payslips).await;
    assert_eq!(
        after, before,
        "a redelivered fact event must leave every payslip stream byte-identical: \
         same record count, same stream sequences, same global sequences, same \
         payloads"
    );
    for (key, stream) in payslips.iter().zip(&after) {
        assert_eq!(
            stream.len(),
            1,
            "payslip {key} must still hold exactly one issue record after redelivery"
        );
    }

    assert_eq!(
        load_stream(&store, run_id.as_str()).await.len(),
        1,
        "redelivery must not add records to the stream that owns the decision"
    );
    assert_eq!(
        issued(&store, &payslips).await.len(),
        EMPLOYEE_COUNT,
        "redelivery must not issue payslips beyond the ones the fact event named"
    );
}

/// The caller-side reading of `OCC-2`.
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
        "a payslip's issue is its genesis record, so OCC-2 applies to it"
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
