//! The child aggregate the fan-out creates.
//!
//! Issuing the payslip is the only thing that happens here, and it is
//! idempotent: that is what lets the pattern deliver at-least-once and still
//! need no compensation.

use serde::{Deserialize, Serialize};

use nitinol_eventsource::{Aggregate, Decider, Decision, Event, Query};
use nitinol_persistence::{EventType, Family, TypeName};

/// The payslip's genesis event.
///
/// It carries no payload because it has nothing to say that the stream it is
/// written to does not already say: the payslip's identity — which run, which
/// employee — *is* its stream key.  Repeating that inside the payload would
/// make the same fact readable from two places that could disagree.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PayslipIssued;

impl Event for PayslipIssued {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("saga.fanout"), TypeName::new("PayslipIssued"));
}

/// Issue the payslip this stream stands for.
///
/// `payslip` is the target's stream key.  It is not read by [`Decider::decide`]
/// — the only thing the decision turns on is whether this payslip has been
/// issued already — but it is what makes the command routable on its own: a
/// reference to the target is built from it with
/// [`EventSourceSystem::aggregate_proxy`](nitinol_eventsource::system::EventSourceSystem::aggregate_proxy),
/// and after a full process crash the saga's crash-restart factory rebuilds that
/// reference out of these very bytes (see
/// [`crate::saga::FanOutSaga::crash_restart_intent`]).
///
/// `Clone + Serialize + Deserialize` are required by
/// `SagaEffect::tell`, which serializes the command into the
/// `TellRequested` outbox marker as its crash-restart payload.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IssuePayslip {
    pub payslip: String,
}

/// One employee's payslip for one payroll run.
///
/// `issued` is the whole state, and it is what makes a redelivered
/// [`IssuePayslip`] a no-op.
#[derive(Default)]
pub struct Payslip {
    issued: bool,
}

impl Payslip {
    /// Stream key of the payslip `payroll_run` owes `employee`.
    ///
    /// Both parts are load-bearing.  An employee outlives any one run, so a key
    /// derived from the employee alone would make the next run's issue land on
    /// a stream whose genesis record is already taken — and this aggregate
    /// would answer that with "already issued", silently withholding a payslip
    /// that was genuinely owed.  Keying by run *and* employee is what confines
    /// the idempotence to redelivery of the same decision.
    ///
    /// This derivation is the single owner of that format: the saga addresses
    /// children through it, and so does every reader that wants to find one.
    pub fn stream_key(payroll_run: &str, employee: &str) -> String {
        format!("{payroll_run}/payslip/{employee}")
    }
}

impl Aggregate for Payslip {
    type Event = PayslipIssued;

    fn apply(&mut self, _event: PayslipIssued) {
        self.issued = true;
    }
}

impl Decider<IssuePayslip> for Payslip {
    /// The issue asks nothing back — the saga dispatches it with `tell` — and
    /// whether it happened is read from the payslip's own stream with
    /// [`IsIssued`].
    type Output = ();
    type Rejection = std::convert::Infallible;

    fn decide(&self, _cmd: IssuePayslip) -> Decision<PayslipIssued, (), Self::Rejection> {
        if self.issued {
            // Already issued — the second delivery of an issue is the success
            // answer, not a conflict to compensate.  An acceptance that states
            // no facts is exactly that: the command found nothing left to do, so
            // nothing is appended and it is still accepted.  A resident payslip
            // answers it from `issued`; a payslip restored by replay answers it
            // from its own stream, because replay is what sets `issued`.
            return Decision::persist(Vec::new()).output(());
        }
        Decision::persist(vec![PayslipIssued]).output(())
    }
}

/// Ask a payslip whether it has been issued yet.
pub struct IsIssued;

impl Query<IsIssued> for Payslip {
    type Response = bool;
    type Error = std::convert::Infallible;

    fn query(&self, _msg: IsIssued) -> Result<bool, Self::Error> {
        Ok(self.issued)
    }
}
