//! The contract between the `nitinol` runtime and downstream domain code.
//!
//! | Item | Purpose |
//! |---|---|
//! | [`Event`] | Marker for domain events; carries their persisted identity |
//! | [`Aggregate`] | Domain state holder; evolves state by applying events |
//! | [`Snapshotable`] | Opt-in snapshot support for faster replay |
//! | [`Decider`] | Decides what a command means for the current state |
//! | [`Decision`] | A decider's conclusion: facts and an answer, or a refusal |
//! | [`Query`] | Asks the current state a question |
//!
//! Every one of them is pure: no I/O, no `async`, and therefore no async
//! runtime. They live here rather than in `nitinol-eventsource` so that a
//! domain-layer crate can define and property-test its aggregates, decisions
//! and queries against the framework's contract without taking on the execution
//! machinery — or Tokio — that runs them. `nitinol-eventsource` re-exports all
//! of them, so the framework-side paths are unchanged.
//!
//! The execution-side abstractions — the aggregate activation, `Codec`, the
//! projection layer, the error families — are deliberately *not* here: they
//! describe how the runtime drives an aggregate, not what an aggregate is or
//! what it decides.
//!
//! # Laws
//!
//! A decision says what happened; it does not say who writes it down. Anything
//! that reads a [`Decision`] and carries it out — the event-sourced runtime, a
//! test harness, a future interpreter that does not exist yet — is an
//! *interpreter*. The laws below are what makes any two correct interpreters
//! observationally equivalent: a domain written against this contract cannot
//! tell them apart, and may therefore be tested against the cheapest one.
//!
//! Laws L-1 to L-4 constrain what a domain may write here. Laws L-5 to L-9
//! constrain the interpreter, and are stated here because they are the promises
//! a domain is entitled to rely on when it writes a decider — the contract
//! itself cannot enforce them.
//!
//! - **L-1** — [`Decider::decide`] and [`Query::query`] are pure and
//!   deterministic: no I/O, no clock, no randomness. Replaying the same events
//!   into the same state must reach the same decision, or replay would not
//!   reconstruct the past.
//! - **L-2** — the order of the events in [`Decision::Accept`] is the order
//!   [`Aggregate::apply`] receives them, and an interpreter persists them as a
//!   single atomic append. Events that do not commute would otherwise land in a
//!   stream that replays into a state the decider never described.
//! - **L-3** — `Accept { events: [], output }` is a legitimate acceptance:
//!   nothing is appended, and the output is delivered as usual. This is how a
//!   command that finds its work already done stays idempotent without
//!   fabricating an event or borrowing the vocabulary of refusal.
//! - **L-4** — a [`Decision::Reject`] is accompanied by no persistence
//!   whatsoever. A refusal is a statement about a command, not a fact about the
//!   aggregate, and leaves no trace in the stream.
//! - **L-5** — on the ask path the output is delivered exactly once. The tell
//!   path discards the output — nobody is waiting for it — but still surfaces a
//!   rejection observably, so that a command silently refused is not mistaken
//!   for one carried out.
//! - **L-6** — `Rejection` carries domain-rule violations only. Infrastructure
//!   and concurrency-control failures are not verdicts on the command and are
//!   reported by the interpreter's own error family (`AskError` in
//!   `nitinol-eventsource`), so that a caller can tell "the domain refused
//!   this" from "this never reached the domain".
//! - **L-7** — when creation collides with an aggregate that already exists, an
//!   interpreter does not fabricate an output; it reports the collision
//!   (`AskError::AlreadyCreated`). No decision was reached, so there is no
//!   answer to deliver, and inventing one would let a caller believe it created
//!   what someone else did.
//! - **L-8** — `sequence` and `occurred_at` are the machine's coordinates. The
//!   interpreter assigns them when it persists, and neither the contract nor
//!   the domain observes them; a domain that needs a time or an order states it
//!   as its own fact in an event.
//! - **L-9** — the aggregate identifier is a domain fact. State owns it, having
//!   received it through a creation event, rather than holding it as a handle
//!   the machinery passed in.
//!
//! # One home for the contract
//!
//! [`Decider`] is the only decision contract in the framework. The effectful
//! trait of the same name that `nitinol-eventsource` used to define — async,
//! handed a `Context`, returning an `Effect` ADT the runtime then interpreted —
//! is gone rather than kept alongside this one. Two deciders would have meant
//! two answers to "what does a command mean", and the effectful one answered it
//! by describing machinery: it let a decision perform I/O, reach for the
//! aggregate's identity and sequence number, and fan out into several appends,
//! none of which a domain rule needs and all of which a replay must reproduce.
//!
//! Nor is a new `nitinol-core` crate introduced to hold the pure contract: this
//! crate already *is* the runtime-free layer, and a second one would leave two
//! plausible homes for every future contract.

mod aggregate;
mod decider;
mod decision;
mod event;
mod query;

pub use self::aggregate::{Aggregate, Snapshotable};
pub use self::decider::Decider;
pub use self::decision::{Accepting, Decision};
pub use self::event::Event;
pub use self::query::Query;
