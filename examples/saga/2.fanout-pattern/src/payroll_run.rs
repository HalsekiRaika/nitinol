//! The aggregate that owns the decision.
//!
//! Approving a payroll run is one decision, and a decision belongs to exactly
//! one stream.  Recording it as a single `PayrollRunApproved` on the run's own
//! stream is what keeps the fan-out inside the framework's axiom: an aggregate
//! is the consistency boundary, so anything that must be atomic has to fit in
//! one append to one stream.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use nitinol_eventsource::{Aggregate, Context, Decider, Effect, Event};
use nitinol_persistence::{EventType, Family, TypeName};

/// The fact event: "this payroll run was approved for these employees".
///
/// # Why the event names its own run
///
/// [`FanOutSaga`](crate::saga::FanOutSaga)'s
/// [`Saga::correlate`](nitinol_saga::Saga::correlate) receives the decoded
/// event and nothing else — not the stream key it came from — so an event that
/// omitted `payroll_run` could not name the process instance it belongs to, and
/// could not name the payslip streams either, since a payslip is addressed by
/// run *and* employee.  Carrying the deciding stream's own key makes the trigger
/// self-sufficient: every consumer derives the whole fan-out from the record
/// alone, without a side channel back to where it was read from.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PayrollRunApproved {
    /// Stream key of the payroll run that was approved.
    pub payroll_run: String,
    /// The employees the approved run covers — one payslip each.
    pub employee_ids: Vec<String>,
}

impl Event for PayrollRunApproved {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("saga.fanout"),
        TypeName::new("PayrollRunApproved"),
    );
}

/// The decision owner.
///
/// The aggregate keeps no state: the fan-out reads the approval from the
/// stream, never from this instance, so holding a copy here would be a second
/// place the same fact lives.
#[derive(Default)]
pub struct PayrollRun;

impl Aggregate for PayrollRun {
    type Event = PayrollRunApproved;

    fn apply(&mut self, _event: PayrollRunApproved) {}
}

/// Approve this run for the employees named by `employee_ids`.
pub struct ApprovePayrollRun {
    pub employee_ids: Vec<String>,
}

#[async_trait]
impl Decider<ApprovePayrollRun> for PayrollRun {
    type Rejection = std::convert::Infallible;

    async fn decide(
        &self,
        cmd: ApprovePayrollRun,
        ctx: &mut Context,
    ) -> Result<Effect<PayrollRunApproved>, Self::Rejection> {
        // One approval, one event, one append — the whole atomicity the pattern
        // claims.  Splitting this into one event per employee would spread the
        // decision over several appends and leave a crash able to record half
        // of it.
        Ok(Effect::persist(PayrollRunApproved {
            payroll_run: ctx.aggregate_id().as_str().to_owned(),
            employee_ids: cmd.employee_ids,
        }))
    }
}
