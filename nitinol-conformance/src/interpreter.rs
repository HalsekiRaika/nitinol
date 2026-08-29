use std::sync::Arc;

use futures_core::future::BoxFuture;
use nitinol_contract::{Decider, Query};
use nitinol_persistence::store::EventStore;
use nitinol_persistence::AggregateId;

use crate::fixture::{Ledger, LedgerNotOpen, LedgerRejection};
use crate::outcome::{Fault, Interpreted, Unanswered};

/// How an interpreter is brought up for one ledger.
///
/// The suite owns the stream key and the store, so that it can seed a history,
/// read back what was recorded, and wedge the store when a clause needs the
/// machinery to fail.  An interpreter that supplied its own store could answer
/// the suite's questions out of its own account of what it did.
pub trait Interpretation: Send + Sync {
    type Interpreter: Interpreter;

    fn interpret(
        &self,
        ledger: AggregateId,
        store: Arc<dyn EventStore>,
    ) -> BoxFuture<'_, Self::Interpreter>;
}

/// One ledger, reached through the machinery under test.
///
/// The methods return a boxed future rather than being `async fn` because they
/// are generic over the command or question: an `async fn` in a trait yields an
/// opaque future whose `Send`ness the suite cannot state, and the suite spawns
/// its work on the caller's runtime.
pub trait Interpreter: Send + Sync {
    /// Carry out `cmd` and deliver its answer.
    fn ask<C>(&self, cmd: C) -> BoxFuture<'_, Interpreted<<Ledger as Decider<C>>::Output>>
    where
        Ledger: Decider<C, Rejection = LedgerRejection>,
        C: Send + Sync + 'static,
        <Ledger as Decider<C>>::Output: Send + 'static;

    /// Carry out `cmd` with nobody waiting for its answer.
    ///
    /// `Ok(())` says the command was taken, not that the ledger accepted it: a
    /// refusal reached on this path has no caller to be returned to and is
    /// surfaced through [`surfaced_refusals`](Interpreter::surfaced_refusals)
    /// instead.  The error is for the machinery failing to take the command at
    /// all.
    fn tell<C>(&self, cmd: C) -> BoxFuture<'_, Result<(), Fault>>
    where
        Ledger: Decider<C, Rejection = LedgerRejection>,
        C: Send + Sync + 'static;

    /// Put `msg` to the ledger's current state.
    fn exec<M>(&self, msg: M) -> BoxFuture<'_, Result<<Ledger as Query<M>>::Response, Unanswered>>
    where
        Ledger: Query<M, Error = LedgerNotOpen>,
        M: Send + Sync + 'static,
        <Ledger as Query<M>>::Response: Send + 'static;

    /// Return once everything handed over so far has been carried out.
    ///
    /// The suite needs a point it can be sure a told command has been dealt
    /// with, and a point it can be sure the interpreter has finished reading
    /// the history before another writer touches the stream.  Waiting on a
    /// clock would prove neither, so an implementation makes whatever
    /// round-trip its own dispatch order guarantees this with; an interpreter
    /// that carries every dispatch out inline has nothing to wait for.
    fn quiesce(&self) -> BoxFuture<'_, ()>;

    /// The refusals this interpreter surfaced for commands nobody was waiting
    /// for.
    ///
    /// How a refusal is surfaced differs from one interpreter to the next — a
    /// log record, a channel, a counter — so what the suite reads is the one
    /// thing they share: each element must contain the refusal's own rendering,
    /// so that a command silently dropped can be told from one that was
    /// refused.
    fn surfaced_refusals(&self) -> Vec<String>;
}
