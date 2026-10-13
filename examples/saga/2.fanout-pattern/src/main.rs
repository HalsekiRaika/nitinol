//! Runnable demonstration of the pattern this crate exists to show:
//! `ApprovePayrollRun` -> one fact event -> one saga decision -> a delivery host
//! -> 32 idempotent payslip issues.
//!
//! The wiring mirrors what `tests/common/helpers.rs` builds for the
//! integration tests; this binary demonstrates it end to end with a
//! self-verifying assertion, in the same shape as
//! `examples/saga/1.basic-saga/src/main.rs`.
//!
//! Three processes do the work, and it is worth naming them separately because
//! they fail and restart independently:
//!
//! * the **payroll run aggregate**, which records the approval;
//! * the **saga manager**, which subscribes to that record and has one
//!   `FanOutSaga` instance persist the fan-out decision;
//! * the **delivery host**, which polls the saga's decisions, asks
//!   `FanOutSaga::dispatch` what each one implies, and delivers the 32 commands.
//!
//! Only the first two existed in the push-based model; the third is what carries
//! a decision to its targets now, and what keeps the record of how far it got.

use std::sync::Arc;
use std::time::Duration;

use nitinol_eventsource::system::EventSourceSystem;
use nitinol_persistence::provider::{CustomStore, PersistenceProvider};
use nitinol_persistence::store::{EventStore, InMemoryCheckpointStore, InMemoryEventStore};
use nitinol_persistence::AggregateId;
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{SagaDeliveryExt, SagaSystemExt};

use saga_fanout_pattern::payroll_run::{ApprovePayrollRun, PayrollRun, PayrollRunApproved};
use saga_fanout_pattern::payslip::{IsIssued, Payslip};
use saga_fanout_pattern::saga::FanOutSaga;

/// Fan-out width the pattern is built around: one approval covers 32 employees.
const EMPLOYEE_COUNT: usize = 32;

/// How often the delivery host asks the store what the sagas have decided.
///
/// Short, because this binary is waiting on it; a deployment trades latency for
/// load on the store here and the framework cannot know which way.
const POLL_INTERVAL: Duration = Duration::from_millis(25);

fn employee_ids() -> Vec<String> {
    (0..EMPLOYEE_COUNT)
        .map(|index| format!("employee-{index:02}"))
        .collect()
}

#[tokio::main]
async fn main() {
    let ps = ProcessSystem::new().await;

    // One supplier for the whole example.  `EventStore` is stream-keyed, so the
    // payroll run's stream, the 32 payslip streams and the saga's journal are
    // tenants of the one journal it supplies; the manager's subscription below
    // is what narrows it back down to the run's stream.
    //
    // The delivery-checkpoint slot is the second thing the supplier owes: it is
    // where the host records how far it has finished, and naming it is what
    // makes `deliver` below exist at all.  A store without it is a compile
    // error at the wiring rather than a failure at the first save.
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let system = EventSourceSystem::builder(ps)
        .with_persistence(PersistenceProvider::new(
            CustomStore::builder()
                .eventstore(store)
                .delivery_checkpoints(InMemoryCheckpointStore::default())
                .build(),
        ))
        .build();

    let run_id = AggregateId::new("payroll-run-april");
    let employees = employee_ids();

    let payroll_run = system.spawn_aggregate::<PayrollRun>(run_id.clone()).await;

    // `host` turns a subscription into one saga instance per correlation key,
    // spawning it on the first event that correlates to it.  One `observing`
    // call per source the saga's alphabet declares — here the run's stream,
    // read as `PayrollRunApproved` — and `spawn` exists only once every
    // declared source has one.
    //
    // The factory ignores what it is handed: a saga that only persists its own
    // decisions needs no reference to anything, since addressing the children is
    // a pure function of the decision and resolving them is the host's work.
    let _manager = system
        .host::<FanOutSaga>(|_components| FanOutSaga)
        .observing::<PayrollRunApproved>(system.subscription(&run_id))
        .spawn()
        .await;

    // The delivery host, started by `deliver`.  It subscribes across *every*
    // instance of `FanOutSaga` by the event type the saga declares — not to one
    // stream — because what it consumes is "the decisions of this saga type",
    // wherever they were made and whichever incarnation made them.  Nothing is
    // named here: the journal it reads, the slot it records progress in and the
    // system it delivers through all come from the system these props were
    // taken from.
    let _delivery = system
        .deliver::<FanOutSaga>()
        .with_poll_interval(POLL_INTERVAL)
        .spawn()
        .await;

    payroll_run
        .ask(ApprovePayrollRun {
            payroll_run: run_id.as_str().to_owned(),
            employee_ids: employees.clone(),
        })
        .await
        .expect("ask(ApprovePayrollRun) must succeed");

    let payslips: Vec<String> = employees
        .iter()
        .map(|employee| Payslip::stream_key(run_id.as_str(), employee))
        .collect();

    // Poll until every payslip reports itself issued. A payslip's own stream is
    // the only place its issue becomes observable, so this polls that fact
    // directly rather than sleeping for a guessed duration.
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let issued_count = loop {
        let mut count = 0;
        for key in &payslips {
            let proxy = system.aggregate_proxy::<Payslip>(AggregateId::new(key));
            if proxy
                .exec(IsIssued)
                .await
                .expect("exec(IsIssued) must succeed")
            {
                count += 1;
            }
        }
        if count == EMPLOYEE_COUNT {
            break count;
        }
        if std::time::Instant::now() >= deadline {
            panic!("fan-out must issue {EMPLOYEE_COUNT} payslips within 20 seconds (had {count})");
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    };

    assert_eq!(
        issued_count, EMPLOYEE_COUNT,
        "every employee named by the fact event must have been issued a payslip"
    );

    println!("fan-out example finished: {issued_count} payslips issued from one fact event");
}
