//! A fan-out interrupted part-way must finish on replay.
//!
//! The first incarnation runs against a store that refuses to append to a chosen
//! subset of the payslip streams, and to record anything on the saga's journal
//! beyond the append that opened it — the durable state a process that dies
//! mid-fan-out leaves behind: some payslips issued, the rest missing, and no
//! record that any dispatch settled.  Refusing by name is what lets one store back
//! the whole incarnation: the payroll run's own stream keeps working, so the
//! decision that drives the fan-out is recorded exactly as in a healthy run.
//!
//! The second incarnation starts fresh (new `ProcessSystem`, new payslip
//! processes, an unwrapped store) over the same durable state.  What completes the
//! fan-out is the durable intent the first incarnation left: every issue is still
//! a pending outbox marker, and replay re-dispatches it — rebuilt from the marker's
//! own bytes, since nothing of the first incarnation is in memory.  The fact event
//! is delivered again too, and dropped before `Saga::handle`: its position is on
//! the saga's own stream.
//!
//! This is the reason the fan-out needs no compensation: the interrupted work is
//! finished by re-running it, because issuing a payslip is idempotent.

#[path = "common/helpers.rs"]
mod common;
use common::{
    decisions, employee_ids, fingerprints, issued, load_stream, payslip_keys, spawn_world,
    wait_for_issued, wait_for_records, InterruptedFanOutStore, DECISION_BLOCK_LEN, EMPLOYEE_COUNT,
};

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use nitinol_eventsource::Event;
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::AggregateId;

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
const SURVIVING_PAYSLIPS: usize = 8;

#[tokio::test]
async fn replay_completes_the_payslips_an_interrupted_fanout_never_issued() {
    // One store holds the payroll run's stream, the saga's journal and all 32
    // payslip streams; both incarnations below are wired to this same instance,
    // which is what makes the second one a replay over the first one's state.
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());

    let run_id = AggregateId::new("payroll-run-crash-replay");
    let employees = employee_ids();
    let payslips = payslip_keys(run_id.as_str(), &employees);
    let (survivors, missing) = payslips.split_at(SURVIVING_PAYSLIPS);
    let survivors = survivors.to_vec();
    let missing = missing.to_vec();
    let saga_key = FanOutSaga::instance_id(run_id.as_str());

    // Run 1 — the fan-out is cut off part-way.

    let interrupted_store = InterruptedFanOutStore::new(
        Arc::clone(&store),
        saga_key.as_str().to_owned(),
        missing.iter().cloned().collect(),
    );

    let first_run = spawn_world(
        &run_id,
        Arc::clone(&interrupted_store) as Arc<dyn EventStore>,
    )
    .await;

    first_run
        .payroll_run
        .ask(ApprovePayrollRun {
            payroll_run: run_id.as_str().to_owned(),
            employee_ids: employees.clone(),
        })
        .await
        .expect("ask(ApprovePayrollRun) must succeed");

    wait_for_issued(&store, &survivors, SURVIVING_PAYSLIPS, FANOUT_TIMEOUT).await;
    let refused = interrupted_store
        .wait_until_refused(EMPLOYEE_COUNT - SURVIVING_PAYSLIPS, FANOUT_TIMEOUT)
        .await;

    assert_eq!(
        refused,
        missing.iter().cloned().collect::<HashSet<String>>(),
        "the interrupted run must have reached — and failed on — exactly the \
         payslips it was not allowed to issue"
    );
    assert_eq!(
        issued(&store, &payslips).await,
        survivors,
        "the interrupted run must leave a genuinely partial fan-out: the allowed \
         payslips issued, the rest absent"
    );
    assert_eq!(
        load_stream(&store, run_id.as_str()).await.len(),
        1,
        "the interrupted run must still have recorded its decision: refusing \
         payslip appends must not touch the stream that owns the decision"
    );

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
        "the interrupted run must leave durable intent and nothing more: the decision, \
         its {EMPLOYEE_COUNT} outbox markers and the position of the fact it was \
         derived from, with no marker recording a dispatch as settled (journal holds \
         {} records)",
        unsettled.len()
    );

    let survivor_records = fingerprints(&store, &survivors).await;

    // Stopping the manager cascade-stops its poller and its saga instances.
    // The payslip processes belong to the system, not to the manager, so run 2
    // deliberately builds a new system rather than reusing this one — a replay
    // must restore payslip state from the store, not inherit it from memory.
    first_run
        .manager
        .stop()
        .await
        .expect("stopping the manager must succeed");

    // Run 2 — replay over the same durable state, with the store unwrapped.

    // Bound for the rest of the test: this handle owns the second incarnation's
    // `ProcessSystem`, and the assertions below observe the state it produces.
    let _second_run = spawn_world(&run_id, Arc::clone(&store)).await;

    wait_for_issued(&store, &payslips, EMPLOYEE_COUNT, FANOUT_TIMEOUT).await;

    let all_records = fingerprints(&store, &payslips).await;
    for (key, stream) in payslips.iter().zip(&all_records) {
        assert_eq!(
            stream.len(),
            1,
            "payslip {key} must hold exactly one issue record after replay"
        );
        assert_eq!(
            stream[0].0, 1,
            "payslip {key}'s issue must occupy its genesis sequence"
        );
    }

    assert_eq!(
        &all_records[..SURVIVING_PAYSLIPS],
        survivor_records.as_slice(),
        "replay must not rewrite the payslips the interrupted run already \
         issued: same stream sequences, same global sequences, same payloads"
    );

    for key in &missing {
        let stream = load_stream(&store, key).await;
        assert_eq!(
            stream[0].event_type,
            PayslipIssued::EVENT_TYPE,
            "payslip {key} must have been completed by the replay with its own \
             issue event"
        );
    }

    let saga_stream = load_stream(&store, saga_key.as_str()).await;
    assert_eq!(
        decisions(&saga_stream).len(),
        1,
        "the fan-out must have been completed by the pending markers, not by a second \
         decision: the redelivered fact event's position is on the saga's own stream, \
         so the framework drops it before `handle`"
    );

    assert_eq!(
        load_stream(&store, run_id.as_str()).await.len(),
        1,
        "replaying the fan-out must not add records to the stream that owns the \
         decision"
    );
}
