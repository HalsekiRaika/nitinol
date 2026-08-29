//! The laws of the `nitinol` contract, made executable for any interpreter.
//!
//! A [`Decision`](nitinol_contract::Decision) says what happened; it does not
//! say who writes it down. Anything that reads one and carries it out — the
//! event-sourced runtime, a test harness, an executor that does not exist yet —
//! is an *interpreter*, and the laws stated in the documentation of
//! [`nitinol_contract`] are what make any two correct interpreters
//! observationally equivalent. This crate turns those laws from prose into a
//! suite you can run against yours.
//!
//! [`verify`] drives one clause per law. It supplies the domain — the
//! [`Ledger`], its commands, its questions and its facts — because a suite that
//! let each interpreter bring its own decider would grade every interpreter
//! against a different domain. It also supplies the store and the stream key,
//! and reads the resulting stream back and decodes it itself, so no clause ever
//! depends on an interpreter agreeing with its own account of what it did.
//!
//! ```rust,ignore
//! # use std::sync::Arc;
//! # use futures_core::future::BoxFuture;
//! # use nitinol_conformance::{Interpretation, Interpreter, Ledger};
//! # use nitinol_persistence::store::EventStore;
//! # use nitinol_persistence::AggregateId;
//! # struct MyMachinery;
//! # struct MyInterpreter;
//! impl Interpretation for MyMachinery {
//!     type Interpreter = MyInterpreter;
//!
//!     fn interpret(
//!         &self,
//!         ledger: AggregateId,
//!         store: Arc<dyn EventStore>,
//!     ) -> BoxFuture<'_, Self::Interpreter> {
//!         Box::pin(async move { /* bring one `Ledger` up on `store` */ })
//!     }
//! }
//!
//! #[tokio::test]
//! async fn my_executor_conforms() {
//!     nitinol_conformance::verify(&MyMachinery).await;
//! }
//! ```
//!
//! # What an interpreter owes the suite
//!
//! Implementing [`Interpretation`] and [`Interpreter`] is the whole of the
//! wiring, and three parts of it are worth stating plainly.
//!
//! **Classify each raw outcome once.** [`Interpreted`] and [`Unanswered`] are
//! the vocabulary every clause judges, and the conversion into them belongs at
//! the interpreter's own boundary, in one place. An interpreter that let each
//! clause reach into a raw error would be graded on the suite's reading of its
//! machinery rather than on the machinery.
//!
//! **Wire the facts to your own store boundary.** [`LedgerEvent::encode`] and
//! [`LedgerEvent::decode`] are the format the suite reads a stream back in. An
//! interpreter with a codec of its own delegates to them; it need agree with
//! the suite about nothing else.
//!
//! **Give [`Interpreter::quiesce`] a real synchronisation point.** The suite
//! needs to know when a told command has been dealt with, and when the
//! interpreter has finished reading a history — never by waiting on a clock. An
//! interpreter that dispatches through a queue makes whatever round-trip its
//! own ordering guarantees; one that carries every dispatch out inline has
//! nothing to wait for and returns immediately.
//!
//! # What this crate may not depend on
//!
//! No interpreter, and no async runtime. `nitinol-eventsource`,
//! `nitinol-runtime` and `tokio` are absent from this crate's dependency tree
//! on purpose: a suite that reached for one interpreter's machinery would be
//! measuring that machinery instead of the laws every interpreter owes, and
//! would be out of reach of anyone writing their own. Whichever runtime you
//! await [`verify`] on is yours to choose.

mod counting_store;
mod fixture;
mod interpreter;
mod outcome;
mod suite;
mod wedged_store;

pub use self::fixture::{
    Balance, Holder, Ledger, LedgerEvent, LedgerNotOpen, LedgerRejection, MalformedLedgerEvent,
    Open, Settle,
};
pub use self::interpreter::{Interpretation, Interpreter};
pub use self::outcome::{Fault, Interpreted, Unanswered};
pub use self::suite::verify;
