//! Wallet aggregate — demonstrates multiple `Decider<C>` on a single aggregate.
//!
//! The same `Wallet` type implements:
//! - `Decider<Deposit>` — adds to the balance
//! - `Decider<Withdraw>` — deducts from the balance, with a rejection if insufficient funds
//! - `Query<GetBalance>` — read-only query for the current balance

use serde::{Deserialize, Serialize};

use nitinol::eventsource::Event;
use nitinol_eventsource::{Aggregate, Decider, Decision, Query};

// Events

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Deposited {
    pub amount: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Withdrawn {
    pub amount: u64,
}

// An aggregate has exactly one event type, so a wallet that records two kinds of
// fact names them as variants of one enum and folds both in `apply`.  A single
// decision may state several of them at once — `Decision::persist` takes a `Vec`
// — and they reach the stream as one atomic append.

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Event)]
#[serde(tag = "kind")]
#[event(family = "multiple_deciders.wallet")]
pub enum WalletEvent {
    Deposited(Deposited),
    Withdrawn(Withdrawn),
}

// Aggregate state

#[derive(Default)]
pub struct Wallet {
    pub balance: u64,
}

impl Aggregate for Wallet {
    type Event = WalletEvent;

    fn apply(&mut self, event: WalletEvent) {
        match event {
            WalletEvent::Deposited(e) => self.balance += e.amount,
            WalletEvent::Withdrawn(e) => self.balance -= e.amount,
        }
    }
}

// Commands

pub struct Deposit {
    pub amount: u64,
}

pub struct Withdraw {
    pub amount: u64,
}

pub struct GetBalance;

// Rejection types

#[derive(Debug, thiserror::Error)]
#[error("insufficient funds: balance {balance} < amount {amount}")]
pub struct InsufficientFunds {
    pub balance: u64,
    pub amount: u64,
}

// Decider / Query implementations

impl Decider<Deposit> for Wallet {
    /// The balance the deposit leaves behind.
    type Output = u64;
    type Rejection = std::convert::Infallible;

    fn decide(&self, cmd: Deposit) -> Decision<WalletEvent, u64, Self::Rejection> {
        let event = WalletEvent::Deposited(Deposited { amount: cmd.amount });
        Decision::persist(vec![event]).output(self.balance + cmd.amount)
    }
}

impl Decider<Withdraw> for Wallet {
    /// The balance the withdrawal leaves behind.
    type Output = u64;
    type Rejection = InsufficientFunds;

    fn decide(&self, cmd: Withdraw) -> Decision<WalletEvent, u64, Self::Rejection> {
        if self.balance < cmd.amount {
            // A domain rule refused the command: no fact was produced and there
            // is no answer to give, which is all a rejection has to say.
            return Decision::reject(InsufficientFunds {
                balance: self.balance,
                amount: cmd.amount,
            });
        }
        let event = WalletEvent::Withdrawn(Withdrawn { amount: cmd.amount });
        Decision::persist(vec![event]).output(self.balance - cmd.amount)
    }
}

impl Query<GetBalance> for Wallet {
    type Response = u64;
    type Error = std::convert::Infallible;

    fn query(&self, _msg: GetBalance) -> Result<u64, Self::Error> {
        Ok(self.balance)
    }
}
