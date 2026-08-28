//! Runnable demonstration of the pattern this crate exists to show:
//! `ApprovePayrollRun` -> one fact event -> saga manager delivery -> 32
//! idempotent payslip issues.
//!
//! The wiring mirrors what `tests/common/helpers.rs` builds for the
//! integration tests; this binary demonstrates it end to end with a
//! self-verifying assertion, in the same shape as
//! `examples/saga/1.basic-saga/src/main.rs`.

use std::sync::Arc;
use std::time::Duration;

use nitinol_eventsource::system::EventSourceSystem;
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::AggregateId;
use nitinol_runtime::ProcessSystem;
use nitinol_saga::SagaDefaultStoreExt;

use saga_fanout_pattern::codec::JsonCodec;
use saga_fanout_pattern::payroll_run::{ApprovePayrollRun, PayrollRun};
use saga_fanout_pattern::payslip::{IsIssued, Payslip};
use saga_fanout_pattern::saga::FanOutSaga;

/// Fan-out width the pattern is built around: one approval covers 32 employees.
const EMPLOYEE_COUNT: usize = 32;

fn employee_ids() -> Vec<String> {
    (0..EMPLOYEE_COUNT)
        .map(|index| format!("employee-{index:02}"))
        .collect()
}

#[tokio::main]
async fn main() {
    let ps = ProcessSystem::new().await;

    // One store for the whole example.  `EventStore` is stream-keyed, so the
    // payroll run's stream, the 32 payslip streams and the saga's journal are
    // tenants of this one instance; the manager's subscription below is what
    // narrows it back down to the run's stream.
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let system = EventSourceSystem::builder(ps)
        .with_codec::<JsonCodec>()
        .with_event_store(store)
        .build();

    let run_id = AggregateId::new("payroll-run-april");
    let employees = employee_ids();

    let payroll_run = system.spawn_aggregate::<PayrollRun>(run_id.clone()).await;

    // The system is itself the shared handle, so the producer and the
    // crash-restart factory each take a clone of it rather than a shared
    // wrapper around it.
    let system_for_producer = system.clone();
    let system_for_factory = system.clone();

    // The manager, not `spawn_saga`, is what turns a subscription into one saga
    // instance per correlation key, spawning it on the first event that
    // correlates to it.
    let _manager = system
        .saga_manager_props(system.subscription(&run_id), move || {
            FanOutSaga::new(system_for_producer.clone())
        })
        .with_crash_restart_factory(move |payload: &[u8]| {
            FanOutSaga::crash_restart_intent(&system_for_factory, payload)
        })
        .spawn(system.process_system())
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
