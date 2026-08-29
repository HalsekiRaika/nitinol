//! A fan-out interrupted part-way must finish on replay.
//!
//! The first incarnation runs against a store that refuses to append to a
//! chosen subset of the payslip streams, so the fan-out stops with some
//! payslips issued and the rest missing — the durable state a crash leaves
//! behind.  Refusing by name is what lets one store back the whole incarnation:
//! the payroll run's own stream and the saga's journal keep working, so the
//! decision that drives the fan-out is recorded exactly as in a healthy run.
//!
//! The second incarnation starts fresh (new `ProcessSystem`, new payslip
//! processes, an unwrapped store) over the same durable state.  Its manager
//! cursor begins at zero, so the fact event is delivered again, and the pattern
//! has to complete the missing issues without disturbing the ones that already
//! exist.
//!
//! This is the reason the fan-out needs no compensation: the interrupted work
//! is finished by re-running it, because issuing a payslip is idempotent.

#[path = "common/helpers.rs"]
mod common;
use common::{
    employee_ids, fingerprints, issued, load_stream, payslip_keys, spawn_world, wait_for_issued,
    PartialFailurePayslipStore, EMPLOYEE_COUNT,
};

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use nitinol_eventsource::Event;
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::AggregateId;

use saga_fanout_pattern::payroll_run::ApprovePayrollRun;
use saga_fanout_pattern::payslip::PayslipIssued;

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

    // Run 1 — the fan-out is cut off part-way.

    let interrupted_store =
        PartialFailurePayslipStore::new(Arc::clone(&store), missing.iter().cloned().collect());

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

    assert_eq!(
        load_stream(&store, run_id.as_str()).await.len(),
        1,
        "replaying the fan-out must not add records to the stream that owns the \
         decision"
    );
}
