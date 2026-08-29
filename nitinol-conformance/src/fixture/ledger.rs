use nitinol_contract::{Aggregate, Decider, Decision, Query};

use crate::fixture::command::{Open, Settle};
use crate::fixture::event::LedgerEvent;
use crate::fixture::question::{Balance, Holder};
use crate::fixture::refusal::{LedgerNotOpen, LedgerRejection};

/// The domain the suite drives an interpreter with.
///
/// Small enough to read at a glance and shaped so that each law has something
/// to bite on: a creation that can collide, an acceptance whose facts do not
/// commute, an acceptance with no facts, a refusal, a question with no answer,
/// and a holder that only a creation event can have supplied.
#[derive(Default)]
pub struct Ledger {
    holder: Option<String>,
    balance: u64,
}

impl Aggregate for Ledger {
    type Event = LedgerEvent;

    fn apply(&mut self, event: LedgerEvent) {
        match event {
            LedgerEvent::Opened { holder } => self.holder = Some(holder),
            LedgerEvent::Credited { amount } => self.balance = self.balance.saturating_add(amount),
            LedgerEvent::Debited { amount } => self.balance = self.balance.saturating_sub(amount),
        }
    }
}

impl Decider<Open> for Ledger {
    type Output = ();
    type Rejection = LedgerRejection;

    fn decide(&self, cmd: Open) -> Decision<LedgerEvent, (), LedgerRejection> {
        if self.holder.is_some() {
            return Decision::reject(LedgerRejection::AlreadyOpen);
        }
        Decision::persist(vec![LedgerEvent::Opened { holder: cmd.holder }]).output(())
    }
}

impl Decider<Settle> for Ledger {
    type Output = u64;
    type Rejection = LedgerRejection;

    fn decide(&self, cmd: Settle) -> Decision<LedgerEvent, u64, LedgerRejection> {
        if self.holder.is_none() {
            return Decision::reject(LedgerRejection::NotOpen);
        }

        let funded = self.balance.saturating_add(cmd.credit);
        if cmd.charge > funded {
            return Decision::reject(LedgerRejection::Underfunded {
                requested: cmd.charge,
                available: funded,
            });
        }

        let mut facts = Vec::new();
        if cmd.credit > 0 {
            facts.push(LedgerEvent::Credited { amount: cmd.credit });
        }
        if cmd.charge > 0 {
            facts.push(LedgerEvent::Debited { amount: cmd.charge });
        }
        Decision::persist(facts).output(funded - cmd.charge)
    }
}

impl Query<Balance> for Ledger {
    type Response = u64;
    type Error = LedgerNotOpen;

    fn query(&self, _: Balance) -> Result<u64, LedgerNotOpen> {
        match self.holder {
            Some(_) => Ok(self.balance),
            None => Err(LedgerNotOpen),
        }
    }
}

impl Query<Holder> for Ledger {
    type Response = String;
    type Error = LedgerNotOpen;

    fn query(&self, _: Holder) -> Result<String, LedgerNotOpen> {
        self.holder.clone().ok_or(LedgerNotOpen)
    }
}
