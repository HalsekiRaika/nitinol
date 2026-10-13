//! The official idempotent-receive pattern: an operation id carried as domain
//! data, absorbed by the receiving aggregate.
//!
//! # Why a receiver has to absorb a repeat
//!
//! A saga cannot send a command.  [`nitinol_saga::SagaEffect`] persists the
//! saga's own decisions and sets the saga's own deadlines, and that is the whole
//! of it.  What a decision means for everybody else is the separate pure mapping
//! [`nitinol_saga::Saga::dispatch`] answers, and carrying what it answers
//! belongs to a **delivery host**: a pull consumer of the saga's own stream that
//! reads each decision, derives the commands it implies, and asks the aggregates
//! they are addressed to.
//!
//! Delivery is at-least-once, and the reason is stated once on
//! [`nitinol_saga::SagaDeliveryProps`]: the host carries a record's deliveries
//! first and moves its offset past that record only once every one of them
//! has landed, so a host that died in between resumes from the offset it
//! last recorded and carries the same deliveries again.  Nothing closes that
//! window — exactly-once would need the receiver's write and the host's
//! offset in one transaction across two stores — and the framework attaches
//! no metadata and no causation envelope to what it hands over.  So recognising
//! a repeat is the receiver's job, using the domain's own operation id, and this
//! crate is that receiving half: what a domain does so that two deliveries of
//! one operation have the effect of one.
//!
//! # The pattern
//!
//! 1. **The operation id is domain data on the decision.**
//!    [`saga::ReservationRequested`] carries `reservation` — and `stock`, whom
//!    the command is addressed to — as ordinary fields.  It *has* to: a delivery
//!    host keeps no saga resident, and [`nitinol_saga::Saga::dispatch`] is
//!    handed the decision event and nothing else, so an id that was not written
//!    into the decision does not exist by the time the command is built.  In
//!    this model the lesson is structural rather than advisory.
//! 2. **The sender derives it deterministically.**
//!    [`saga::ReservationSaga::reservation_id`] is a pure function of the
//!    upstream event, so a decision made again from that event — by an
//!    incarnation whose predecessor's append never landed, or by two activations
//!    of one instance deciding before either append is durable — names the id
//!    the first one named.  A command re-derived from a decision already written
//!    carries it for a simpler reason: the id is on the record.
//! 3. **The receiver records the ids it applied on its own stream.**
//!    [`stock_item::StockReserved`] carries the operation id, so
//!    [`stock_item::StockItem`] rebuilds the applied set by replaying its
//!    events rather than holding it in the activation that applied it.
//! 4. **A repeat is accepted, stating no facts.**  [`stock_item::StockItem`]'s
//!    decider answers a known id with `Decision::persist(Vec::new()).output(())`
//!    — an acceptance that appends nothing, so a caller can still tell "already
//!    done" apart from "not allowed".  It has to be an acceptance: a host holds
//!    on a delivery the receiver refused and retries it rather than passing it
//!    over, so a repeat answered with a rejection would stall the partition on
//!    an operation that is already done.
//!
//! How far back an applied-id set is kept is an application decision this crate
//! does not make: dropping an id is the same as forgetting that the operation
//! happened, so the horizon belongs in the domain — a reservation that expires,
//! an order that closes — rather than in an eviction policy chosen for memory
//! alone.
//!
//! # The shape of the example
//!
//! An order is placed; a saga reacts to that one fact event and persists one
//! decision, "this order's reservation was asked for"; a delivery host reads
//! that decision and asks the stock item to reserve against it.  The stock item
//! is the receiver, and the reservation's operation id is derived from the
//! order.
//!
//! Three wirings, each named where it is needed: the aggregates on the system,
//! the saga manager subscribed to the order's stream
//! ([`nitinol_saga::SagaSystemExt`]), and the delivery host over the saga's own
//! decisions ([`nitinol_saga::SagaDeliveryExt`], or
//! [`nitinol_saga::SagaDeliveryProps`] where the stores are named by hand).
//!
//! `EventStore` is keyed by stream, so the order's stream, the stock item's
//! stream and the saga's own journal are tenants of one store instance; the
//! manager's subscription is what narrows it back down to the order's stream,
//! and the host's is what narrows it down to the saga's own decision event type.
//! A delivery host also needs somewhere to keep how far it has got, which is a
//! second slot on the store and nothing to do with the journal.
//!
//! `src/main.rs` runs the whole thing end to end; the tests in `tests/` fix the
//! behaviour it demonstrates.

pub mod codec;
pub mod order;
pub mod saga;
pub mod stock_item;
