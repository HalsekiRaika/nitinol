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
//! ```rust,no_run
//! # use futures_core::future::BoxFuture;
//! # use nitinol_conformance::{
//! #     Fault, Interpretation, Interpreted, Interpreter, Ledger, LedgerNotOpen, LedgerRejection,
//! #     Unanswered,
//! # };
//! # use nitinol_contract::{Decider, Query};
//! # use nitinol_persistence::store::Journal;
//! # use nitinol_persistence::AggregateId;
//! # struct MyMachinery;
//! # struct MyInterpreter;
//! # impl Interpreter for MyInterpreter {
//! #     fn ask<C>(&self, cmd: C) -> BoxFuture<'_, Interpreted<<Ledger as Decider<C>>::Output>>
//! #     where
//! #         Ledger: Decider<C, Rejection = LedgerRejection>,
//! #         C: Send + Sync + 'static,
//! #         <Ledger as Decider<C>>::Output: Send + 'static,
//! #     {
//! #         unimplemented!()
//! #     }
//! #     fn tell<C>(&self, cmd: C) -> BoxFuture<'_, Result<(), Fault>>
//! #     where
//! #         Ledger: Decider<C, Rejection = LedgerRejection>,
//! #         C: Send + Sync + 'static,
//! #     {
//! #         unimplemented!()
//! #     }
//! #     fn exec<M>(&self, msg: M) -> BoxFuture<'_, Result<<Ledger as Query<M>>::Response, Unanswered>>
//! #     where
//! #         Ledger: Query<M, Error = LedgerNotOpen>,
//! #         M: Send + Sync + 'static,
//! #         <Ledger as Query<M>>::Response: Send + 'static,
//! #     {
//! #         unimplemented!()
//! #     }
//! #     fn quiesce(&self) -> BoxFuture<'_, ()> {
//! #         unimplemented!()
//! #     }
//! #     fn surfaced_refusals(&self) -> Vec<String> {
//! #         unimplemented!()
//! #     }
//! # }
//! impl Interpretation for MyMachinery {
//!     type Interpreter = MyInterpreter;
//!
//!     fn interpret(
//!         &self,
//!         ledger: AggregateId,
//!         store: Journal,
//!     ) -> BoxFuture<'_, Self::Interpreter> {
//!         Box::pin(async move { MyInterpreter })
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
//! **The facts' format is not yours to choose.** [`LedgerEvent`] names
//! [`LedgerCodec`] as the codec that writes it down, so an interpreter reaches
//! the suite's format through `<LedgerEvent as Event>::Codec` and has nothing
//! to wire. [`LedgerEvent::encode`] and [`LedgerEvent::decode`] are the same
//! format, reachable directly when a store boundary wants the bytes without the
//! trait.
//!
//! **Give [`Interpreter::quiesce`] a real synchronisation point.** The suite
//! needs to know when a told command has been dealt with, and when the
//! interpreter has finished reading a history — never by waiting on a clock. An
//! interpreter that dispatches through a queue makes whatever round-trip its
//! own ordering guarantees; one that carries every dispatch out inline has
//! nothing to wait for and returns immediately.
//!
//! # Async runtime
//!
//! This crate depends on no async runtime, so [`verify`] can be awaited on
//! whichever runtime you choose.

#![warn(missing_docs)]

mod counting_store;
mod fixture;
mod interpreter;
mod outcome;
mod suite;
mod wedged_store;

pub use self::fixture::{
    Balance, Holder, Ledger, LedgerCodec, LedgerEvent, LedgerNotOpen, LedgerRejection,
    MalformedLedgerEvent, Open, Settle,
};
pub use self::interpreter::{Interpretation, Interpreter};
pub use self::outcome::{Fault, Interpreted, Unanswered};
pub use self::suite::verify;
