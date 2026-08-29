//! The domain the suite drives an interpreter with.
//!
//! An interpreter under test does not write its own aggregate: it wires the
//! [`Ledger`] defined here to whatever machinery it interprets a
//! [`Decision`](nitinol_contract::Decision) with.  That is what lets two
//! interpreters that share nothing else be compared — and what stops a suite
//! from grading an interpreter against a decider chosen to suit it.

mod command;
mod event;
mod ledger;
mod question;
mod refusal;

pub use self::command::{Open, Settle};
pub use self::event::{LedgerEvent, MalformedLedgerEvent};
pub use self::ledger::Ledger;
pub use self::question::{Balance, Holder};
pub use self::refusal::{LedgerNotOpen, LedgerRejection};
