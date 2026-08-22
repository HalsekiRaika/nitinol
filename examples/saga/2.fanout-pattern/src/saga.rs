//! The process manager that turns one approval into many payslips.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use nitinol_eventsource::system::{EventSourceSystem, StoreSet};
use nitinol_eventsource::{AggregateProxy, Event};
use nitinol_persistence::{AggregateId, EventType, Family, TypeName};
use nitinol_saga::{Saga, SagaContext, SagaEffect, SagaId, TellIntent};

use crate::codec::JsonCodec;
use crate::payroll_run::PayrollRunApproved;
use crate::payslip::{IssuePayslip, Payslip};

/// The system every payslip in this example lives on.
type PayrollSystem = EventSourceSystem<JsonCodec, StoreSet>;

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
    type SubscribedEvent = PayrollRunApproved;
    type Event = FanOutStarted;
    type ScheduledMessage = ();
    type Error = std::convert::Infallible;

    fn correlate(event: &Self::SubscribedEvent) -> Option<SagaId> {
        Some(Self::instance_id(&event.payroll_run))
    }

    /// The saga keeps no state.
    ///
    /// How far the fan-out got is already recorded — each payslip's own stream
    /// says whether it exists.  Mirroring that into the saga would give the
    /// same fact two owners that a crash between them could leave disagreeing,
    /// and the disagreement would be invisible until the next replay.
    fn apply(&mut self, _event: Self::Event) {}

    /// A redelivered fact event produces the same decision again, on purpose.
    ///
    /// Suppressing the second fan-out here would put the idempotence in the
    /// saga, where it holds only as long as this incarnation's memory does.
    /// Leaving it in the payslips puts it behind their durable streams
    /// instead, so it survives the crash it exists to protect against.
    async fn handle(
        &mut self,
        event: Self::SubscribedEvent,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
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
