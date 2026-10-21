//! The child aggregate the fan-out creates.
//!
//! Issuing the payslip is the only thing that happens here, and it is
//! idempotent: that is what lets the pattern deliver at-least-once and still
//! need no compensation.
//!
//! Nothing in this module knows a saga exists.  A delivery host reaches the
//! payslip through the same reference any other caller would resolve, so the
//! receiving side of a fan-out is an ordinary [`Aggregate`] + [`Decider`] pair
//! and implements no framework surface of its own.  What it does owe is the
//! absorption of a repeat, because the delivery that brings the command is
//! at-least-once.

use serde::{Deserialize, Serialize};

use nitinol_eventsource::{Aggregate, Decider, Decision, Event, Never, Query};
use nitinol_persistence::{EventType, Family, TypeName};

use crate::codec::JsonCodec;

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

    type Codec = JsonCodec;
}

/// Issue the payslip this stream stands for.
///
/// It carries nothing, for the same reason [`PayslipIssued`] does: *which*
/// payslip is being issued is the stream key of the aggregate the command is
/// addressed to, and the address is named beside the command rather than inside
/// it — [`SagaDelivery::new`](nitinol_saga::SagaDelivery::new) takes an
/// [`AggregateId`](nitinol_persistence::AggregateId) and the command as two
/// arguments.
///
/// # Why there is no operation id on it
///
/// A receiver under at-least-once delivery has to recognise a repeat, and the
/// general answer is an operation id the domain puts on the command — that is
/// what `examples/saga/3.idempotent-receive` is about.  A creation-only fan-out
/// is the degenerate case of it: this payslip is issued at most once in its
/// whole life, so "have I done this already?" and "do I exist already?" are the
/// same question, and the payslip answers it from its own genesis record.  An
/// operation id here would be a second copy of the stream key.
///
/// # What the command is *not* required to be
///
/// Neither `Clone` nor serializable.  A delivery host writes down no copy of a
/// command: what is durable is the saga's decision, and the commands are
/// re-derived from it by [`Saga::dispatch`](nitinol_saga::Saga::dispatch) every
/// time the host reads that decision.  So nothing about a command has to survive
/// a restart — only the decision does.
pub struct IssuePayslip;

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
    type Recover = Never;

    fn apply(&mut self, _event: PayslipIssued) {
        self.issued = true;
    }
}

impl Decider<IssuePayslip> for Payslip {
    /// The issue has nothing to answer back, but it is still delivered with
    /// `ask`: the delivery host needs the receiver's answer to know the command
    /// was carried out, since a refusal and a success would otherwise be the
    /// same event and the host would treat the decision as finished either way.
    /// What the issue *means* is read from the payslip's own stream, with
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
