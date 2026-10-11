//! The fan-out pattern under at-least-once delivery.
//!
//! One decision is recorded as one fact event on one stream; the saga turns that
//! fact into one decision of its own naming all 32 payslips; a delivery host
//! turns that decision into 32 issue commands.  The pattern's claim is that
//! carrying those 32 commands *again* changes nothing observable in the payslip
//! streams — the idempotence lives in the payslips, not in a short-circuit
//! inside the saga, and not in bookkeeping the host keeps per command.
//!
//! Delivery is at-least-once because a host's checkpoint moves only after the
//! deliveries a record implied have landed, and the two writes are to two stores.
//! A repeat is not only a crash's doing, though, and the clause below uses the
//! other documented way of producing one: a *second host over the same journal*,
//! filing its progress under an identifier of its own.  That is the shape of a
//! second deployment, or of a migration running beside the first, and it starts
//! from nothing — so it reads the decision the first host already finished with
//! and carries every one of its commands a second time.
//!
//! The fact event is redelivered too, because the manager's cursor is not
//! durable, and it is *not* decided again: its position is on the saga's own
//! stream, committed by the append that carried the decision, so the framework
//! drops it before `Saga::handle`.
//!
//! The store-side half of the idempotence claim — a conflict on a stream's
//! genesis sequence means "already created", and the first write stays intact —
//! is stated and fixed by `nitinol-persistence`.  What is fixed here is the
//! caller-side half the `EventStore::append` contract leaves to convention: a
//! redelivered issue writes nothing new and destroys nothing old.
//!
//! Every stream these tests observe — the payroll run's, the 32 payslips' and
//! the saga's own — lives in the one store the incarnation was given, which is
//! what the assertions below read from.

#[path = "common/helpers.rs"]
mod common;
use common::{
    decision_position, decisions, delivered_through, delivery_scope, drain_payslips, employee_ids,
    fingerprint, fingerprints, issued, load_stream, payslip_keys, spawn_delivery_host, spawn_world,
    wait_for_delivery_through, wait_for_issued, DECISION_BLOCK_LEN, EMPLOYEE_COUNT,
};

use std::sync::Arc;
use std::time::Duration;

use nitinol_eventsource::Event;
use nitinol_persistence::error::AppendError;
use nitinol_persistence::store::{EventStore, InMemoryCheckpointStore, InMemoryEventStore};
use nitinol_persistence::{AggregateId, AppendingEvent};
use nitinol_saga::SagaDeliveryId;

use saga_fanout_pattern::payroll_run::{ApprovePayrollRun, PayrollRunApproved};
use saga_fanout_pattern::payslip::{Payslip, PayslipIssued};
use saga_fanout_pattern::saga::{FanOutSaga, FanOutStarted};

/// Budget for a 32-wide fan-out to reach the store.  The manager polls its
/// upstream every 250ms and the host polls the saga's decisions on its own
/// cadence, so this is generous rather than tight — a deadline, not a
/// synchronisation device.
const FANOUT_TIMEOUT: Duration = Duration::from_secs(20);

/// The one journal an incarnation is wired with.
///
/// `EventStore` is stream-keyed, so a single instance holds the run's stream,
/// the payslip streams and the saga's journal side by side; splitting them
/// across instances is a deployment choice, not something the pattern needs.
fn single_store() -> Arc<dyn EventStore> {
    Arc::new(InMemoryEventStore::default())
}

/// One fact, one decision of two rows, 32 payslips.
///
/// This pins the shape the rest of the pattern rests on: the decision is a
/// single record on the payroll run's stream (not a per-employee write, and not
/// a write spanning several streams), the saga's own stream holds that decision
/// and the consumed position of the fact it came from — and *nothing per
/// child* — and the 32 payslips exist on streams of their own.
#[tokio::test]
async fn one_fact_event_records_the_approval_and_fans_out_to_thirty_two_payslips() {
    let store = single_store();
    let run_id = AggregateId::new("payroll-run-single-append");
    let employees = employee_ids();
    let payslips = payslip_keys(run_id.as_str(), &employees);

    let world = spawn_world(
        &run_id,
        Arc::clone(&store),
        InMemoryCheckpointStore::default(),
        &SagaDeliveryId::new("payslip-delivery"),
    )
    .await;

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

    // The saga's own stream is where the durable intent lives, and it is two
    // rows wide however many children the fan-out owes.  `append(Vec)` stamps one
    // `occurred_at` over a whole commit unit, so a decision that did not share
    // its append with the consumed position would show two timestamps.
    let saga_key = FanOutSaga::instance_id(run_id.as_str());
    let saga_stream = load_stream(&store, saga_key.as_str()).await;
    assert_eq!(
        saga_stream.len(),
        DECISION_BLOCK_LEN,
        "the saga's stream must hold exactly the decision and the consumed position \
         of the fact it was derived from. A {EMPLOYEE_COUNT}-wide fan-out adds no \
         row per child: what each child is owed is re-derived from the decision by \
         `FanOutSaga::dispatch` every time a host reads it (journal holds {} records)",
        saga_stream.len()
    );
    assert_eq!(
        saga_stream.iter().map(|e| e.sequence).collect::<Vec<u64>>(),
        (1..=DECISION_BLOCK_LEN as u64).collect::<Vec<u64>>(),
        "the decision block must occupy contiguous sequences from the genesis \
         sequence — a gap or an interleaved record means the decision and the \
         position of its input did not share one append"
    );
    assert_eq!(
        saga_stream[0].event_type,
        FanOutStarted::EVENT_TYPE,
        "the saga's decision event must lead its own stream"
    );
    assert_eq!(
        decisions(&saga_stream).len(),
        1,
        "the decision block must carry exactly one domain event; the other record \
         is the framework's — the consumed position of the fact event"
    );
    assert!(
        saga_stream
            .iter()
            .all(|e| e.occurred_at == saga_stream[0].occurred_at),
        "every record in the decision block must carry the timestamp of the one \
         append that committed it"
    );

    // One decision carrying all 32 addresses is what lets a host with no saga
    // resident produce the whole fan-out from that record alone.
    let decided: FanOutStarted = serde_json::from_slice(&saga_stream[0].payload)
        .expect("the decision event must decode with the codec it declares");
    assert_eq!(
        decided.payslips, payslips,
        "the decision must name every payslip the fan-out owes, in order: \
         `Saga::dispatch` is handed this event and nothing else, so an address \
         missing from it is a command nobody can derive"
    );
}

/// Every command carried a second time must leave the payslips untouched.
///
/// A second delivery host over the same journal, filing its progress under an
/// identifier of its own, starts from nothing and therefore reads the decision
/// the first host already finished with.  It re-derives all 32 commands from it
/// and carries them again — which is precisely what a host resuming from a
/// checkpoint a crash left behind does, with none of a crash's non-determinism.
///
/// The payslip processes stay resident across it, so the repeats are absorbed in
/// their plain form — no aggregate replay involved.
#[tokio::test]
async fn commands_carried_a_second_time_write_no_second_payslip() {
    let store = single_store();
    let run_id = AggregateId::new("payroll-run-redelivery");
    let employees = employee_ids();
    let payslips = payslip_keys(run_id.as_str(), &employees);
    let saga_key = FanOutSaga::instance_id(run_id.as_str());

    let first_host = SagaDeliveryId::new("payslip-delivery");
    let second_host = SagaDeliveryId::new("payslip-delivery-again");
    let checkpoints = InMemoryCheckpointStore::default();

    let world = spawn_world(
        &run_id,
        Arc::clone(&store),
        checkpoints.clone(),
        &first_host,
    )
    .await;

    world
        .payroll_run
        .ask(ApprovePayrollRun {
            payroll_run: run_id.as_str().to_owned(),
            employee_ids: employees.clone(),
        })
        .await
        .expect("ask(ApprovePayrollRun) must succeed");

    wait_for_issued(&store, &payslips, EMPLOYEE_COUNT, FANOUT_TIMEOUT).await;

    // The first host's progress reaching the decision's position is what says it
    // finished every delivery that decision implied — so from here it has nothing
    // left to do, and anything the payslip streams gain is the second host's.
    let decision = decision_position(&store, saga_key.as_str()).await;
    wait_for_delivery_through(
        &checkpoints,
        &delivery_scope(&first_host),
        decision,
        FANOUT_TIMEOUT,
    )
    .await;

    let before = fingerprints(&store, &payslips).await;

    // The second host's progress is filed under a key of its own, so it holds
    // nothing. That is what makes the clause below a genuine repeat rather than a
    // host with nothing left to read: it starts from the beginning of the journal
    // and finds the decision the first host finished with.
    assert_eq!(
        delivered_through(&checkpoints, &delivery_scope(&second_host)).await,
        None,
        "a host named for the first time must have no progress recorded, so what it \
         reads is the whole journal — the decision included"
    );

    let _second_delivery = spawn_delivery_host(&world.system, &second_host).await;

    // The second host recording the same position is the observable point: it has
    // carried all 32 commands over again and had every one of them answered.
    wait_for_delivery_through(
        &checkpoints,
        &delivery_scope(&second_host),
        decision,
        FANOUT_TIMEOUT,
    )
    .await;

    let states = drain_payslips(&world.system, &payslips).await;
    assert_eq!(
        states,
        vec![true; EMPLOYEE_COUNT],
        "every payslip must report itself issued after the commands were carried \
         a second time"
    );

    let after = fingerprints(&store, &payslips).await;
    assert_eq!(
        after, before,
        "a repeated issue must leave every payslip stream byte-identical: same \
         record count, same stream sequences, same global sequences, same payloads"
    );
    for (key, stream) in payslips.iter().zip(&after) {
        assert_eq!(
            stream.len(),
            1,
            "payslip {key} must still hold exactly one issue record after the repeat"
        );
    }

    let saga_stream = load_stream(&store, saga_key.as_str()).await;
    assert_eq!(
        saga_stream.len(),
        DECISION_BLOCK_LEN,
        "carrying the commands again must add nothing to the saga's own stream: a \
         host writes down no record of what it delivered, and the redelivered fact \
         event is dropped before `handle` because its position is already there \
         (journal holds {} records)",
        saga_stream.len()
    );
    assert_eq!(
        decisions(&saga_stream).len(),
        1,
        "the redelivered fact event must not be decided a second time: its position \
         is on the saga's own stream, so the framework drops it before `handle`"
    );

    assert_eq!(
        load_stream(&store, run_id.as_str()).await.len(),
        1,
        "the repeat must not add records to the stream that owns the decision"
    );
    assert_eq!(
        issued(&store, &payslips).await.len(),
        EMPLOYEE_COUNT,
        "the repeat must not issue payslips beyond the ones the fact event named"
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

    let world = spawn_world(
        &run_id,
        Arc::clone(&store),
        InMemoryCheckpointStore::default(),
        &SagaDeliveryId::new("payslip-delivery"),
    )
    .await;

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
///
/// This is also where the absence of an operation id on
/// [`IssuePayslip`](saga_fanout_pattern::payslip::IssuePayslip) is paid for: the
/// payslip recognises a repeat by its own existence, so its identity has to be
/// exactly as narrow as the operation it stands for.
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
