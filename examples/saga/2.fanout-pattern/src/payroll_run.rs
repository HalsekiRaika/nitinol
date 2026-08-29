//! The aggregate that owns the decision.
//!
//! Approving a payroll run is one decision, and a decision belongs to exactly
//! one stream.  Recording it as a single `PayrollRunApproved` on the run's own
//! stream is what keeps the fan-out inside the framework's axiom: an aggregate
//! is the consistency boundary, so anything that must be atomic has to fit in
//! one append to one stream.

use serde::{Deserialize, Serialize};

use nitinol_eventsource::{Aggregate, Decider, Decision, Event};
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
///
/// The key reaches the event through [`ApprovePayrollRun`] and, being this
/// stream's genesis record, is what gives the aggregate its own identity in
/// state — a pure decision reads `&self` and its command and is never told which
/// stream it serves.
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
/// The only thing it keeps is the key of the run it was approved for, learned
/// from the genesis event in `apply`.  Nothing about the fan-out is mirrored
/// here: which employees the run covers is read from the stream, never from this
/// instance, so holding a copy of *that* would be a second place the same fact
/// lives.
#[derive(Default)]
pub struct PayrollRun {
    /// The run this aggregate is, once its genesis event has been applied.
    ///
    /// `None` before approval, because until then the stream holds nothing that
    /// names it: a pure decision is handed `&self` and a command and is told
    /// nothing about which stream it serves, so an aggregate that wants its own
    /// identity has to have been given it by a fact it recorded.
    pub payroll_run: Option<String>,
}

impl Aggregate for PayrollRun {
    type Event = PayrollRunApproved;

    fn apply(&mut self, event: PayrollRunApproved) {
        self.payroll_run = Some(event.payroll_run);
    }
}

/// Approve this run for the employees named by `employee_ids`.
///
/// `payroll_run` is the run's own stream key.  It is on the command because the
/// approval is this stream's genesis decision: there is no earlier fact for the
/// aggregate to have learned its identity from, and the decider is not told the
/// key by the machinery around it.
pub struct ApprovePayrollRun {
    pub payroll_run: String,
    pub employee_ids: Vec<String>,
}

impl Decider<ApprovePayrollRun> for PayrollRun {
    /// The approval asks nothing back: what it is worth is the fan-out that
    /// follows, and that becomes observable on the payslips' own streams.
    type Output = ();
    type Rejection = std::convert::Infallible;

    fn decide(&self, cmd: ApprovePayrollRun) -> Decision<PayrollRunApproved, (), Self::Rejection> {
        // One approval, one event, one append — the whole atomicity the pattern
        // claims.  Splitting this into one event per employee would spread the
        // decision over several appends and leave a crash able to record half
        // of it.
        Decision::persist(vec![PayrollRunApproved {
            payroll_run: cmd.payroll_run,
            employee_ids: cmd.employee_ids,
        }])
        .output(())
    }
}
