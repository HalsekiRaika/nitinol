//! The process manager that turns one approval into one decision, and the pure
//! mapping that says what that decision owes the 32 children.
//!
//! The two are deliberately separate methods on one trait:
//!
//! * [`Saga::handle`] decides, and the only thing it can express is "persist
//!   this".  A saga has no way to say "send this command" — [`SagaEffect`] has
//!   no variant for it.
//! * [`Saga::dispatch`] is a pure associated function from one of the saga's own
//!   events to the commands it implies.  A *delivery host* calls it and carries
//!   what it answers.
//!
//! What that split buys is the crash story.  The decision is durable before
//! anything is delivered, and the only progress the host keeps is a checkpoint
//! it moves *after* the deliveries a record implied have landed.  So a host that
//! dies part-way through a fan-out resumes from a checkpoint still standing
//! below the decision, re-derives all 32 commands from it, and carries the
//! remaining ones — plus the ones it already carried, which the payslips absorb.
//! Nothing of the fan-out is durable state that could fall out of step with the
//! decision, because nothing of it is stored at all.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use nitinol_eventsource::Event;
use nitinol_persistence::{AggregateId, EventType, Family, TypeName};
use nitinol_saga::{Saga, SagaContext, SagaDelivery, SagaEffect, SagaId, SagaSubscribe};

use crate::codec::JsonCodec;
use crate::payroll_run::PayrollRunApproved;
use crate::payslip::{IssuePayslip, Payslip};

/// The saga's own decision event: "this fan-out was started for this run, and it
/// owes these payslips".
///
/// # Why it names every child
///
/// [`Saga::dispatch`] is handed this event and nothing else — no `&self`, no
/// stream key, no system.  That is not an oversight: a delivery host keeps no
/// saga resident, and the decision it is reading may well have been written by
/// an incarnation that no longer exists.  So everything the fan-out is addressed
/// to has to be *in* the record, which for a 32-wide fan-out means one event
/// listing all 32 payslip streams.
///
/// It restates the run too, rather than pointing back at the trigger, so the
/// saga's stream can be read on its own — the same reason the trigger carries
/// its run key.
///
/// Contrast [`PayslipIssued`](crate::payslip::PayslipIssued), which carries
/// nothing: *which* payslip that event is about is already its stream key,
/// whereas *which payslips* this fan-out covers is not derivable from the
/// saga's stream key.  An event carries what its stream does not already say,
/// and no more.
///
/// # Why there is no counter beside it
///
/// How far the fan-out got is already recorded — each payslip's own stream says
/// whether it exists.  A progress counter on the saga would be a second owner of
/// that fact, and a crash between the two writes would leave them disagreeing
/// with no way to tell which is right.  One decision, re-derived into commands
/// on every read, is the whole of the durable intent.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FanOutStarted {
    pub payroll_run: String,
    pub payslips: Vec<String>,
}

impl Event for FanOutStarted {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("saga.fanout"), TypeName::new("FanOutStarted"));

    type Codec = JsonCodec;
}

/// The whole alphabet this fan-out reacts to: the run's fact event, and no
/// deadline of its own.
///
/// Everything the saga reacts to is declared here, and everything reaches it
/// through the one [`Saga::handle`] entry point.  A fan-out that later had to
/// notice each payslip's own reply, or give the run a deadline to complete
/// within, would gain a variant here — and the `match`es that answer the
/// alphabet would stop compiling until it was handled.
///
/// Note what is *not* in it: no variant for "a delivery failed".  A command the
/// framework could not carry is transport, and transport belongs to the delivery
/// host — it holds, retries and says so, and the saga is never told.  A
/// *business* refusal would arrive here, as the event the refusing aggregate
/// recorded on its own stream.
#[derive(SagaSubscribe)]
pub enum Observed {
    PayrollRunApproved(PayrollRunApproved),
}

/// Records one decision per payroll run, and maps it to one issue command per
/// payslip.
///
/// It holds nothing.  A saga that only persists its own decisions needs no
/// reference to anything else: addressing is a pure function of the decision
/// ([`Saga::dispatch`]), and resolving the targets is the delivery host's work.
/// That is why the producer handed to the manager ignores its
/// [`SagaComponents`](nitinol_saga::SagaComponents).
#[derive(Default)]
pub struct FanOutSaga;

impl FanOutSaga {
    /// One fan-out process per payroll run.
    ///
    /// This is the single definition of that rule: [`Saga::correlate`] answers
    /// with it, and it names the stream the instance persists to.
    pub fn instance_id(payroll_run: &str) -> SagaId {
        SagaId::new(format!("{payroll_run}/fanout-saga"))
    }
}

#[async_trait]
impl Saga for FanOutSaga {
    type Subscribe = Observed;
    type Event = FanOutStarted;
    type Error = std::convert::Infallible;

    fn correlate(input: &Self::Subscribe) -> Option<SagaId> {
        let Observed::PayrollRunApproved(event) = input;
        Some(Self::instance_id(&event.payroll_run))
    }

    // The saga keeps no state, so `apply` is left to its no-op default.
    //
    // `apply` says what a decision means to the saga itself, and here it means
    // nothing: the saga never consults its own past.  `dispatch` below is the
    // other half of the same pair — what the decision means for everybody else —
    // and that one is the whole point of this type.

    /// What one fan-out decision owes the children.
    ///
    /// Pure, and handed the decision alone.  Every command is built from the
    /// payslip keys the decision listed, so a host reading this record years
    /// later — or in a different process, with no saga anywhere — produces
    /// exactly the same 32 deliveries.
    ///
    /// The order is the order of the list, and the host delivers them one at a
    /// time in it, stopping at the first one that does not land.  A delivery that
    /// is refused is *held*, not passed over: the host stops advancing, retries
    /// the same record, and reports it, because a command dropped for failing is
    /// a decision the saga made that nobody carried out and nothing left to say
    /// so.  That is what makes an interrupted fan-out finishable instead of
    /// quietly incomplete.
    fn dispatch(event: &Self::Event) -> Vec<SagaDelivery> {
        event
            .payslips
            .iter()
            .map(|payslip| {
                SagaDelivery::new::<Payslip, IssuePayslip>(
                    AggregateId::new(payslip.clone()),
                    IssuePayslip,
                )
            })
            .collect()
    }

    /// Nothing here suppresses a repeat, on purpose.
    ///
    /// Two things do, and both are backed by a stream: the framework drops a
    /// fact event whose decision is already durable before this method sees it
    /// (the fact's position is committed by the very append that carries the
    /// decision), and each payslip answers a repeated issue with a write-free
    /// acceptance.  A check inside this method would be backed by neither — it
    /// would hold only as long as this incarnation's memory does, and it would do
    /// nothing at all about the repeats a delivery host makes, which never pass
    /// through here.
    async fn handle(
        &mut self,
        input: Self::Subscribe,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        let Observed::PayrollRunApproved(event) = input;
        let payslips: Vec<String> = event
            .employee_ids
            .iter()
            .map(|employee| Payslip::stream_key(&event.payroll_run, employee))
            .collect();

        // One event, one append — and the position of the fact it was derived
        // from committed in the same batch.  There is nothing per-child to
        // persist, so there is no partial write for a crash to leave behind:
        // either the whole fan-out is decided or none of it is.
        Ok(SagaEffect::persist(FanOutStarted {
            payroll_run: event.payroll_run,
            payslips,
        }))
    }
}
