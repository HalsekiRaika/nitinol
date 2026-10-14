//! A fan-out interrupted part-way must finish when the delivery host is started
//! again.
//!
//! The first incarnation runs against a store that refuses, for good, to append
//! to a chosen tail of the payslip streams.  Its delivery host therefore issues
//! the payslips it can reach, hits the first one it cannot, and holds: it stops
//! advancing, retries the same decision, and records no progress.  That is the
//! durable state a process that dies mid-fan-out leaves behind — some payslips
//! issued, the rest missing, and an offset still standing below the decision
//! that owes them.  Refusing by name is what lets one store back the whole
//! incarnation: the payroll run's own stream and the saga's own journal keep
//! working, so the decision that drives the fan-out is recorded exactly as in a
//! healthy run.
//!
//! The second incarnation starts fresh — new `ProcessSystem`, new payslip
//! processes, an unwrapped store — over the same durable state *and the same
//! delivery offset*.  What completes the fan-out is that offset: it never
//! moved past the decision, so the new host reads the decision again,
//! `FanOutSaga::dispatch` re-derives all 32 commands from it, and the host
//! carries the 24 that never landed together with the 8 that did.  The 8 absorb
//! the repeat with a write-free acceptance.
//!
//! Nothing about the first incarnation's dispatch was persisted, and that is
//! exactly why this works: there is no record of "which commands were sent" to be
//! half-written, so there is no state for the recovery to reconcile.  The
//! decision is the whole of the durable intent, and it is re-read in full.
//!
//! This is also the reason the fan-out needs no compensation: the interrupted
//! work is finished by re-running it, because issuing a payslip is idempotent.
//!
//! # Why the first host does not finish the job later
//!
//! A `SagaDeliveryProxy` has no `stop`: its partitions are processes of the
//! system they were spawned on, and an operator reaches them to move a
//! offset, not to shut one down.  So the first incarnation's host is still
//! running while the second works, held on the ninth payslip — and it stays held,
//! because the store it writes through refuses that append for good.  The 24
//! payslips the assertions below find could therefore have been written by
//! nothing but the second incarnation.

#[path = "common/helpers.rs"]
mod common;
use common::{
    decision_position, decisions, delivered_through, delivery_scope, employee_ids, fingerprints,
    issued, load_stream, payslip_keys, settle, spawn_world, wait_for_delivery_through,
    wait_for_issued, wait_for_records, UnreachablePayslipStore, DECISION_BLOCK_LEN, EMPLOYEE_COUNT,
};

use std::collections::HashSet;
use std::time::Duration;

use nitinol_eventsource::Event;
use nitinol_persistence::store::{InMemoryEventStore, InMemoryOffsetStore, Journal};
use nitinol_persistence::AggregateId;
use nitinol_saga::SagaDeliveryId;

use saga_fanout_pattern::payroll_run::ApprovePayrollRun;
use saga_fanout_pattern::payslip::PayslipIssued;
use saga_fanout_pattern::saga::FanOutSaga;

/// Budget for a 32-wide fan-out to reach the store.  A deadline, not a
/// synchronisation device — every wait below polls an observable condition.
const FANOUT_TIMEOUT: Duration = Duration::from_secs(20);

/// How many payslips the interrupted incarnation is allowed to issue.
///
/// Strictly between zero and [`EMPLOYEE_COUNT`]: at zero the second run would
/// just be a first run, and at `EMPLOYEE_COUNT` there would be nothing left to
/// complete.
///
/// It is also a *prefix* of the fan-out, because `FanOutSaga::dispatch` answers
/// the payslips in the order the decision lists them and the host delivers them
/// in that order, stopping at the first one that does not land.  So the
/// reachable ones have to come first for the interrupted run to issue any at all
/// — which is itself the point the clause below about holding rather than
/// skipping is checking.
const SURVIVING_PAYSLIPS: usize = 8;

#[tokio::test]
async fn a_restarted_delivery_host_completes_the_payslips_an_interrupted_fanout_never_issued() {
    // One store holds the payroll run's stream, the saga's journal and all 32
    // payslip streams; both incarnations below are wired to this same instance,
    // which is what makes the second one a continuation of the first one's state.
    //
    // The backend handle is kept because the second incarnation is wired with it
    // directly, and `store` is the erased handle the assertions read through —
    // one clone of one backend either way, which is what makes the two the one
    // durable state.
    let backend = InMemoryEventStore::default();
    let store = Journal::from(backend.clone());

    // One saga-offset slot, handed to both incarnations under one
    // identifier: the second host is *the same host restarted*, so it resumes
    // from the progress the first one recorded — which, as the clauses below
    // fix, is none.
    let offsets = InMemoryOffsetStore::default();
    let delivery_id = SagaDeliveryId::new("payslip-delivery");
    let scope = delivery_scope(&delivery_id);

    let run_id = AggregateId::new("payroll-run-crash-replay");
    let employees = employee_ids();
    let payslips = payslip_keys(run_id.as_str(), &employees);
    let (survivors, missing) = payslips.split_at(SURVIVING_PAYSLIPS);
    let survivors = survivors.to_vec();
    let missing = missing.to_vec();
    let saga_key = FanOutSaga::instance_id(run_id.as_str());

    // Run 1 — the fan-out is cut off part-way.

    let unreachable = UnreachablePayslipStore::new(
        store.clone(),
        missing.iter().cloned().collect::<HashSet<String>>(),
    );

    let first_run = spawn_world(&run_id, unreachable.clone(), offsets.clone(), &delivery_id).await;

    first_run
        .payroll_run
        .ask(ApprovePayrollRun {
            payroll_run: run_id.as_str().to_owned(),
            employee_ids: employees.clone(),
        })
        .await
        .expect("ask(ApprovePayrollRun) must succeed");

    wait_for_issued(&store, &survivors, SURVIVING_PAYSLIPS, FANOUT_TIMEOUT).await;
    unreachable.wait_until_refused(FANOUT_TIMEOUT).await;

    // The decision is recorded exactly as in a healthy run: refusing payslip
    // appends touches neither the stream that owns the decision nor the saga's.
    assert_eq!(
        load_stream(&store, run_id.as_str()).await.len(),
        1,
        "the interrupted run must still have recorded its decision on the payroll \
         run's stream"
    );
    let journal = wait_for_records(
        &store,
        saga_key.as_str(),
        DECISION_BLOCK_LEN,
        FANOUT_TIMEOUT,
    )
    .await;
    assert_eq!(
        journal.len(),
        DECISION_BLOCK_LEN,
        "the saga's stream must hold the decision and the consumed position of the \
         fact it was derived from, and nothing else: a {EMPLOYEE_COUNT}-wide fan-out \
         persists nothing per child, so there is no partial dispatch record for the \
         interruption to leave behind (journal holds {} records)",
        journal.len()
    );
    let decision = decision_position(&store, saga_key.as_str()).await;

    // Give the held host every chance to carry on past what it cannot deliver.
    settle().await;

    assert_eq!(
        unreachable.refused(),
        [missing[0].clone()]
            .into_iter()
            .collect::<HashSet<String>>(),
        "a delivery that cannot be carried out is held, not skipped: the host must \
         stop at the first payslip it cannot write and keep retrying that one, \
         rather than moving on to the {} behind it",
        missing.len() - 1
    );
    assert_eq!(
        issued(&store, &payslips).await,
        survivors,
        "the interrupted run must leave a genuinely partial fan-out: the reachable \
         prefix issued, the rest absent"
    );
    assert_eq!(
        delivered_through(&offsets, &scope).await,
        None,
        "the interrupted host must have recorded no progress at all: a record's \
         position is written only once every delivery it implied has landed, and \
         24 of these never did. Had it recorded the decision anyway, the restart \
         below would resume past the fan-out it never finished"
    );

    let survivor_records = fingerprints(&store, &survivors).await;

    // Stopping the manager cascade-stops its poller and its saga instances.  The
    // payslip processes belong to the system, not to the manager, so run 2
    // deliberately builds a new system rather than reusing this one — a restart
    // must reach payslips restored from the store, not ones that are still
    // resident with the first incarnation's state.
    first_run
        .manager
        .stop()
        .await
        .expect("stopping the manager must succeed");

    // Run 2 — the same host, restarted over the same durable state and the same
    // offset, with the store unwrapped.

    // Bound for the rest of the test: this handle owns the second incarnation's
    // `ProcessSystem`, and the assertions below observe the state it produces.
    let _second_run = spawn_world(&run_id, backend.clone(), offsets.clone(), &delivery_id).await;

    wait_for_issued(&store, &payslips, EMPLOYEE_COUNT, FANOUT_TIMEOUT).await;

    assert_eq!(
        wait_for_delivery_through(&offsets, &scope, decision, FANOUT_TIMEOUT).await,
        decision,
        "the restarted host must now record the decision's position, which is what \
         says every one of the {EMPLOYEE_COUNT} deliveries it implied has landed"
    );

    let all_records = fingerprints(&store, &payslips).await;
    for (key, stream) in payslips.iter().zip(&all_records) {
        assert_eq!(
            stream.len(),
            1,
            "payslip {key} must hold exactly one issue record after the restart"
        );
        assert_eq!(
            stream[0].0, 1,
            "payslip {key}'s issue must occupy its genesis sequence"
        );
    }

    assert_eq!(
        &all_records[..SURVIVING_PAYSLIPS],
        survivor_records.as_slice(),
        "re-carrying the decision must not rewrite the payslips the interrupted run \
         already issued: same stream sequences, same global sequences, same payloads"
    );

    for key in &missing {
        let stream = load_stream(&store, key).await;
        assert_eq!(
            stream[0].event_type,
            PayslipIssued::EVENT_TYPE,
            "payslip {key} must have been completed by the restarted host with its \
             own issue event"
        );
    }

    let saga_stream = load_stream(&store, saga_key.as_str()).await;
    assert_eq!(
        saga_stream.len(),
        DECISION_BLOCK_LEN,
        "completing the fan-out must add no row to the saga's own stream: what was \
         re-read is the decision that was already there, and the redelivered fact \
         event is dropped before `handle` because its position is on that same \
         stream (journal holds {} records)",
        saga_stream.len()
    );
    assert_eq!(
        decisions(&saga_stream).len(),
        1,
        "the fan-out must have been completed by re-carrying the one decision, not \
         by deciding a second time"
    );

    assert_eq!(
        load_stream(&store, run_id.as_str()).await.len(),
        1,
        "completing the fan-out must not add records to the stream that owns the \
         decision"
    );
}
