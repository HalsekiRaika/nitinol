//! Wallet aggregate — demonstrates multiple `Decider<C>` on a single aggregate.
//!
//! The same `Wallet` type implements:
//! - `Decider<Deposit>` — adds to the balance
//! - `Decider<Withdraw>` — deducts from the balance, with a rejection if insufficient funds
//! - `Receive<GetBalance>` — read-only query for the current balance

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use nitinol_eventsource::{Aggregate, Context, Decider, Effect, Event, Receive as EvtReceive};
use nitinol_persistence::EventType;

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Deposited {
    pub amount: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Withdrawn {
    pub amount: u64,
}

// ---------------------------------------------------------------------------
// A wallet holds multiple event types via the `Sequence` variant of `Effect`.
// The aggregate must fold both event kinds — use an enum as the event union.
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum WalletEvent {
    Deposited(Deposited),
    Withdrawn(Withdrawn),
}

impl Event for WalletEvent {
    // The outer enum carries its own type string; inner variants are embedded.
    const EVENT_TYPE: EventType = EventType::from_str("MultipleDeciders.Wallet.Event");
}

// ---------------------------------------------------------------------------
// Aggregate state
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

pub struct Deposit {
    pub amount: u64,
}

pub struct Withdraw {
    pub amount: u64,
}

pub struct GetBalance;

// ---------------------------------------------------------------------------
// Rejection types
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
#[error("insufficient funds: balance {balance} < amount {amount}")]
pub struct InsufficientFunds {
    pub balance: u64,
    pub amount: u64,
}

// ---------------------------------------------------------------------------
// Decider / Receive implementations
// ---------------------------------------------------------------------------

#[async_trait]
impl Decider<Deposit> for Wallet {
    type Rejection = std::convert::Infallible;

    async fn decide(
        &self,
        cmd: Deposit,
        _ctx: &mut Context,
    ) -> Result<Effect<WalletEvent>, Self::Rejection> {
        Ok(Effect::persist(WalletEvent::Deposited(Deposited {
            amount: cmd.amount,
        })))
    }
}

#[async_trait]
impl Decider<Withdraw> for Wallet {
    type Rejection = InsufficientFunds;

    async fn decide(
        &self,
        cmd: Withdraw,
        _ctx: &mut Context,
    ) -> Result<Effect<WalletEvent>, Self::Rejection> {
        if self.balance < cmd.amount {
            return Err(InsufficientFunds {
                balance: self.balance,
                amount: cmd.amount,
            });
        }
        Ok(Effect::persist(WalletEvent::Withdrawn(Withdrawn {
            amount: cmd.amount,
        })))
    }
}

#[async_trait]
impl EvtReceive<GetBalance> for Wallet {
    type Response = u64;
    type Error = std::convert::Infallible;

    async fn recv(&self, _msg: GetBalance, _ctx: &mut Context) -> Result<u64, Self::Error> {
        Ok(self.balance)
    }
}
