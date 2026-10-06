//! The process manager that turns one approval into many payslips.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use nitinol_eventsource::system::EventSourceSystem;
use nitinol_eventsource::{AggregateProxy, Event};
use nitinol_persistence::provider::Persistence;
use nitinol_persistence::{AggregateId, EventType, Family, TypeName};
use nitinol_saga::{Saga, SagaContext, SagaEffect, SagaId, SagaSubscribe, TellIntent};

use crate::codec::JsonCodec;
use crate::payroll_run::PayrollRunApproved;
use crate::payslip::{IssuePayslip, Payslip};

/// The system every payslip in this example lives on.
///
/// The provider supplies the journal and nothing else: this example wires no
/// projector, no delivery host and no snapshots, so it names no store for those
/// slots and the wiring that would need them does not compile against it.
pub type PayrollSystem = EventSourceSystem<Persistence>;

/// A reference to the payslip that owns the stream `key`.
///
/// A fan-out addresses children that may not be running: the first delivery
/// creates them, and a replay after a crash addresses a set that is partly
/// resident and partly gone.  Routing is therefore "given a stream key, name the
/// aggregate" — which is what an
/// [`AggregateProxy`] already is, so this example holds no registry of its own.
/// The reference is built without awaiting, which is what lets the `fold` below
/// and the crash-restart factory produce one at all.
fn payslip_ref(system: &PayrollSystem, key: &str) -> AggregateProxy<Payslip> {
    system.aggregate_proxy::<Payslip>(AggregateId::new(key))
}

/// The saga's own decision event: "this fan-out was started for this run".
///
/// It restates the run and the payslips it resolved rather than pointing back
/// at the trigger, so the saga's stream can be read on its own — the same
/// reason the trigger carries its run key.
///
/// Contrast [`PayslipIssued`](crate::payslip::PayslipIssued), which carries
/// nothing: *which* payslip that event is about is already its stream key,
/// whereas *which payslips* this fan-out covers is not derivable from the
/// saga's stream key.  An event carries what its stream does not already say,
/// and no more.
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
#[derive(SagaSubscribe)]
pub enum Observed {
    PayrollRunApproved(PayrollRunApproved),
}

/// Delivers one issue command per payslip named by a [`PayrollRunApproved`].
pub struct FanOutSaga {
    system: PayrollSystem,
}

impl FanOutSaga {
    pub fn new(system: PayrollSystem) -> Self {
        Self { system }
    }

    /// One fan-out process per payroll run.
    ///
    /// This is the single definition of that rule: [`Saga::correlate`] answers
    /// with it, and it names the stream the instance persists to.
    pub fn instance_id(payroll_run: &str) -> SagaId {
        SagaId::new(format!("{payroll_run}/fanout-saga"))
    }

    /// Rebuild a pending issue from the bytes a `TellRequested` marker kept
    /// for it.
    ///
    /// After a full process crash the in-memory intent is gone and only the
    /// marker survives, so the command has to be reconstructed from its
    /// serialized form — which is exactly why [`IssuePayslip`] carries the
    /// target stream key.
    ///
    /// `None` is the framework's signal that this marker cannot be
    /// reconstructed; replay then records a `TellFailed` for it instead of
    /// dropping it unnoticed.
    pub fn crash_restart_intent(
        system: &PayrollSystem,
        crash_restart_payload: &[u8],
    ) -> Option<TellIntent> {
        let cmd: IssuePayslip = serde_json::from_slice(crash_restart_payload).ok()?;
        let target = payslip_ref(system, &cmd.payslip);
        Some(TellIntent::new::<Payslip, IssuePayslip, _>(target, cmd))
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
    // How far the fan-out got is already recorded — each payslip's own stream
    // says whether it exists.  Mirroring that into the saga would give the same
    // fact two owners that a crash between them could leave disagreeing, and the
    // disagreement would be invisible until the next replay.

    /// Nothing here suppresses a repeat, on purpose.
    ///
    /// Two things could: the framework drops a fact event whose fan-out is already
    /// durable before this method sees it, and each payslip answers a repeated
    /// issue with a write-free acceptance.  Both are backed by a stream.  A check
    /// inside this method would not be: it would hold only as long as this
    /// incarnation's memory does, and it would do nothing at all about the issues
    /// a pending outbox marker repeats, which never pass through here.
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

        let decision = SagaEffect::persist(FanOutStarted {
            payroll_run: event.payroll_run,
            payslips: payslips.clone(),
        });

        // `combine` folds adjacent `Persist` branches, so the decision and all
        // of its outbox markers reach the store as one append.  A crash after a
        // partial write could otherwise leave the saga believing it had fanned
        // out to payslips it never enqueued.
        Ok(payslips.into_iter().fold(decision, |effect, payslip| {
            let target = payslip_ref(&self.system, &payslip);
            effect.combine(SagaEffect::tell::<Payslip, IssuePayslip, _>(
                target,
                IssuePayslip { payslip },
            ))
        }))
    }
}
