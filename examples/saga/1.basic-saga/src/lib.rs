//! A minimal event-sourced process manager: an order is placed, and the stock
//! for it gets reserved.
//!
//! # The three parts, and why they are three
//!
//! 1. **The saga subscribes.**  [`saga::ReservationSaga`] observes the order's
//!    stream and reacts to `OrderPlaced` in [`Saga::handle`][nitinol_saga::Saga::handle].
//!    It is a pull consumer of that stream, so the order aggregate neither
//!    knows nor waits for it.
//! 2. **The saga decides, and only decides.**  `handle` returns a
//!    [`SagaEffect`][nitinol_saga::SagaEffect], and the only durable thing one
//!    can express is "persist this on my own stream" (plus setting or
//!    cancelling the saga's own deadlines).  There is no "send this command"
//!    effect, so a decision cannot be half-made: it is a row on the saga's
//!    stream or it is nothing.
//! 3. **A delivery host carries the consequences.**
//!    [`Saga::dispatch`][nitinol_saga::Saga::dispatch] is a pure function from
//!    one decision to the commands it implies, and a host spawned in
//!    `src/main.rs` reads the decisions out of the store, calls that mapping,
//!    and `ask`s each addressed aggregate.
//!
//! # What that buys, and what it costs
//!
//! The host is a pull consumer with one piece of state: how far it has
//! finished, recorded as a checkpoint.  It delivers a decision's commands
//! first and moves the checkpoint past the decision only once all of them have
//! landed, so a host that dies in between resumes from the checkpoint and
//! carries the same commands again.  That window is the whole of why delivery
//! is **at-least-once**, and why the decision event carries an operation id the
//! receiver recognises a repeat by — see
//! [`inventory::Inventory`]'s `Decider<Reserve>`.
//!
//! The other half of the bargain: a delivery the receiver refuses is not passed
//! over.  The host holds — it stops advancing its checkpoint and retries the
//! same record — rather than silently dropping a command the saga decided and
//! nobody carried out.
//!
//! # What the pieces look like
//!
//! * [`order`] — the aggregate whose fact starts the process.
//! * [`saga`] — the process manager: the alphabet it observes, the decision it
//!   persists, and the mapping from that decision to a command.
//! * [`inventory`] — the aggregate the command is addressed to.  It implements
//!   no trait the saga introduced; a host reaches it through the same reference
//!   any other caller would.
//! * `src/main.rs` — the wiring: one supplier, the two aggregates, the saga's
//!   subscription, and the delivery host.

pub mod codec;
pub mod inventory;
pub mod order;
pub mod saga;
