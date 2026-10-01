//! The official idempotent-receive pattern: an operation id carried as domain
//! data, absorbed by the receiving aggregate.
//!
//! When a tell can be delivered twice, and what re-dispatch does and does not
//! reproduce, is stated once on [`nitinol_saga::SagaEffect::tell`].  This crate
//! is the receiving half: what a domain does so that two deliveries of one
//! operation have the effect of one.
//!
//! # The pattern
//!
//! 1. **The operation id is domain data on the command.**
//!    [`stock_item::Reserve`] carries `reservation` as an ordinary field,
//!    written by whoever builds the command.
//! 2. **The sender derives it deterministically.**
//!    [`saga::ReservationSaga::reservation_id`] is a pure function of the
//!    upstream event, so a command built again from that event — by an
//!    incarnation whose predecessor's batch never landed, or by another node
//!    holding a duplicate activation — carries the id the first one carried.
//!    A command rebuilt from a pending marker's bytes carries it for a different
//!    reason: those bytes are the ones dispatched the first time.
//! 3. **The receiver records the ids it applied on its own stream.**
//!    [`stock_item::StockReserved`] carries the operation id, so
//!    [`stock_item::StockItem`] rebuilds the applied set by replaying its
//!    events rather than holding it in the activation that applied it.
//! 4. **A repeat is accepted, stating no facts.**  [`stock_item::StockItem`]'s
//!    decider answers a known id with `Decision::persist(Vec::new()).output(())`
//!    — an acceptance that appends nothing, so a caller can still tell "already
//!    done" apart from "not allowed".
//!
//! How far back an applied-id set is kept is an application decision this crate
//! does not make: dropping an id is the same as forgetting that the operation
//! happened, so the horizon belongs in the domain — a reservation that expires,
//! an order that closes — rather than in an eviction policy chosen for memory
//! alone.
//!
//! # The shape of the example
//!
//! An order is placed; a saga reacts to that one fact event and asks a stock
//! item to reserve against it.  The stock item is the receiver, and the
//! reservation's operation id is derived from the order.
//!
//! `EventStore` is keyed by stream, so the order's stream, the stock item's
//! stream and the saga's own journal are tenants of one store instance; the
//! manager's subscription is what narrows it back down to the order's stream.
//!
//! `src/main.rs` runs the whole thing end to end; the tests in `tests/` fix the
//! behaviour it demonstrates.

pub mod codec;
pub mod order;
pub mod saga;
pub mod stock_item;
