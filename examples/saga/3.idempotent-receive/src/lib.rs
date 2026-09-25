//! The official idempotent-receive pattern: an operation id carried as domain
//! data, absorbed by the receiving aggregate.
//!
//! Tell dispatch through a saga's outbox can deliver the same command more than
//! once, and — separately — the same upstream fact can reach
//! [`Saga::handle`](nitinol_saga::Saga::handle) more than once and produce a
//! second command with the same meaning.  The delivery contract that says
//! exactly when each of those happens is stated once, on
//! [`nitinol_saga::SagaEffect::tell`].  This crate is the receiving half: what a
//! domain does so that two deliveries of one operation have the effect of one.
//!
//! The answer is not a framework feature.  nitinol attaches no metadata or
//! causation envelope to a dispatched command — the record an append is handed,
//! the record a load returns and a decider's [`Decision`](nitinol_eventsource::Decision)
//! carry the domain's own bytes and nothing beside them — so a domain that needs
//! to recognise a repeat carries what it needs to recognise it *by*, as its own
//! data.  The same rule the framework applies to identity generally: an
//! identifier a domain acts on is a fact the domain owns, not a coordinate the
//! machine happens to have.
//!
//! # The shape
//!
//! 1. **The operation id is domain data on the command.**
//!    [`stock_item::Reserve`] carries `reservation` as an ordinary field.  It is
//!    written by whoever builds the command and travels inside the payload,
//!    which is the only place a dispatched command carries anything.
//! 2. **The sender derives it deterministically.**
//!    [`saga::ReservationSaga::reservation_id`] is a pure function of the
//!    upstream event.  This is the requirement that is easy to miss: an id
//!    minted per `handle` call survives the replay of one outbox marker and
//!    fails the re-processing of one upstream event, and nothing downstream can
//!    tell the difference until a duplicate has been applied.
//! 3. **The receiver holds the ids it applied as its own state, recorded on its
//!    own stream.**  [`stock_item::StockReserved`] carries the operation id, so
//!    [`stock_item::StockItem`] rebuilds the set by replaying its events.  A set
//!    kept only in the activation that applied it would last exactly as long as
//!    that activation, which is shorter than the redelivery window it exists to
//!    cover.
//! 4. **A repeat is accepted, stating no facts.**
//!    [`stock_item::StockItem`]'s decider answers a known operation id with
//!    `Decision::persist(Vec::new()).output(())` — an acceptance that appends
//!    nothing.  A repeat is a command that found nothing left to do, not a
//!    domain-rule violation, so it must not borrow the vocabulary of refusal:
//!    a caller cannot otherwise distinguish "already done" from "not allowed".
//!
//! # Why the id is stable, and what the framework does not promise
//!
//! Two deliveries of one operation arise in two different ways, and they hold
//! for different reasons.
//!
//! * **The same `TellRequested` re-dispatched.**  The marker's crash-restart
//!   payload is the command's serialized bytes, persisted at dispatch time and
//!   handed back to the crash-restart factory verbatim.  The operation id inside
//!   them is therefore unchanged with no help from the framework and no work by
//!   the domain — it was fixed when the sender built the command.
//! * **The same upstream event handled again.**  Here nothing is re-decoded: a
//!   second incarnation catching up from the start of the order's stream, or
//!   another node processing it concurrently, runs `handle` again and builds a
//!   new command.  Only a derivation that is a function of the event makes the
//!   id come out the same, which is why
//!   [`saga::ReservationSaga::reservation_id`] is one.
//!
//! Those two are also the boundary of what is guaranteed. "The same bytes are
//! re-decoded" is the framework's promise; "the factory reconstructs the same
//! command addressed to the same target" is the *user's* — the framework hands
//! the factory the bytes and does not check what comes back.  A deployment that
//! changes a command's serialized shape, or its factory, across a restart with
//! markers still pending is the same obligation again, and it is the user's.
//!
//! # How far back the receiver remembers
//!
//! An applied-id set that only ever grows is bounded by the stream it is
//! replayed from, and every real deployment eventually wants it bounded smaller.
//! That bound is an application decision this crate deliberately does not make,
//! because dropping an id is exactly the same as forgetting that the operation
//! happened: past the horizon, a redelivery is applied again.  Expressing the
//! horizon as a domain fact of its own — a reservation that expires, an order
//! that closes — is what keeps the two in agreement; a cache eviction policy
//! chosen for memory reasons alone does not.
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
//! # What the tests fix
//!
//! * `tests/duplicate_dispatch_applies_once.rs` — the very same `Reserve`
//!   handed twice to the stock item is accepted and leaves its stream
//!   byte-identical; and a re-dispatch produced by a second manager incarnation
//!   writes no second reservation.
//! * `tests/operation_id_survives_reprocessing.rs` — the id is a function of the
//!   upstream event (stable for one order, distinct across orders), and a second
//!   incarnation over the same durable state converges on one reservation with a
//!   receiver that restored its applied-id set from its own stream.
//! * `../../../tests/dispatch_carries_no_envelope.rs` (in the umbrella crate) —
//!   the records and the decision a dispatched command's facts travel through
//!   carry no metadata field for the framework to have put an id in.

pub mod codec;
pub mod order;
pub mod saga;
pub mod stock_item;
