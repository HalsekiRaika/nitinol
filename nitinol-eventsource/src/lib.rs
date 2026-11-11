//! CQRS + Event Sourcing integration layer for the `nitinol` framework.
//!
//! This crate provides the core abstractions for building event-sourced
//! aggregates:
//!
//! | Trait / Type | Purpose |
//! |---|---|
//! | [`Aggregate`] | Domain state holder; evolves state by applying events |
//! | [`Decider<C>`](Decider) | Decides what a command means for the current state |
//! | [`Decision`] | A decider's conclusion: facts and an answer, or a refusal |
//! | [`Query<M>`](Query) | Read-only question asked of the current aggregate state |
//! | [`Event`] | Marker for domain events; names the codec that writes them |
//! | [`Snapshotable`] | How an aggregate's snapshot value is captured and read back |
//! | [`Recovery`] | What [`Aggregate::Recover`] may name |
//! | [`Never`] | Replay the whole stream; asks the store for nothing but the journal |
//! | [`Latest`] | Recover from the system's newest snapshot, then replay what followed it |
//! | [`AggregateProxy`] | Identity-based reference to an aggregate; resolves a dispatch to an activation and re-resolves after one dies |
//!
//! Every trait and type in that table is defined in `nitinol-contract`, which
//! carries no async runtime, and is re-exported here unchanged: a domain crate
//! can implement them without depending on the execution machinery in this
//! crate.  What this crate adds is the *interpreter* — the activation that reads
//! a [`Decision`], persists its facts and delivers its output under the laws
//! stated in [`nitinol_contract`].
//!
//! # Getting started
//!
//! Running a `Counter` aggregate on an in-memory store and asking it to
//! `Increment`:
//!
//! ```
//! # use std::convert::Infallible;
//! # use bytes::Bytes;
//! # use nitinol_eventsource::codec::Codec;
//! # use nitinol_eventsource::system::EventSourceSystem;
//! # use nitinol_eventsource::{Aggregate, AggregateProxy, Decider, Decision, Event, Never};
//! # use nitinol_persistence::provider::PersistenceProvider;
//! # use nitinol_persistence::store::InMemoryStore;
//! # use nitinol_persistence::{AggregateId, EventType, Family, TypeName};
//! # use nitinol_runtime::ProcessSystem;
//! # #[derive(Clone)]
//! # struct Incremented;
//! # struct IncrementedCodec;
//! # impl Codec<Incremented> for IncrementedCodec {
//! #     type Error = Infallible;
//! #     fn encode(_: &Incremented) -> Result<Bytes, Infallible> { Ok(Bytes::new()) }
//! #     fn decode(_: &[u8]) -> Result<Incremented, Infallible> { Ok(Incremented) }
//! # }
//! # impl Event for Incremented {
//! #     const EVENT_TYPE: EventType =
//! #         EventType::new(Family::new("counter"), TypeName::new("incremented"));
//! #     type Codec = IncrementedCodec;
//! # }
//! # #[derive(Default)]
//! # struct Counter { value: u64 }
//! # impl Aggregate for Counter {
//! #     type Event = Incremented;
//! #     type Recover = Never;
//! #     fn apply(&mut self, _: Incremented) { self.value += 1; }
//! # }
//! # struct Increment;
//! # impl Decider<Increment> for Counter {
//! #     type Output = u64;
//! #     type Rejection = Infallible;
//! #     fn decide(&self, _: Increment) -> Decision<Incremented, u64, Infallible> {
//! #         Decision::persist(vec![Incremented]).output(self.value + 1)
//! #     }
//! # }
//! # #[tokio::main]
//! # async fn main() {
//! let ps = ProcessSystem::new().await;
//! let system = EventSourceSystem::builder(ps)
//!     .with_persistence(PersistenceProvider::new(InMemoryStore::default()))
//!     .build();
//!
//! let counter: AggregateProxy<Counter> =
//!     system.spawn_aggregate(AggregateId::new("counter-1")).await;
//! let value = counter.ask(Increment).await.expect("ask failed");
//! assert_eq!(value, 1);
//! # }
//! ```
//!
//! See the `examples/eventsource` directory for step-by-step examples:
//!
//! 1. `eventsource-basic-aggregate` – minimal counter aggregate
//! 2. `eventsource-multiple-deciders` – multiple commands per aggregate
//! 3. `eventsource-projection` – aggregate and projector (Catch-up and Live)
//! 4. `eventsource-snapshot` – snapshot-accelerated replay
//! 5. `eventsource-aggregate-communication` – inter-aggregate messaging, as a saga
//!
//! # Observing lost races
//!
//! An activation numbers its appends from the sequence it replayed to, and the
//! store refuses an append at a sequence another writer has already taken.  The
//! activation that lost stops, and the [`AggregateProxy`] above it resolves a
//! fresh one, so nothing a caller sees says the race happened.  How often it
//! happens is still worth knowing: it is what decides whether restricting an
//! aggregate to a single activation is worth its cost, and whether an aggregate
//! boundary is drawing more writers than it should.
//!
//! So every such refusal emits one `tracing` event.  Its identity is part of
//! this crate's API — an operator's filter is written against it, so renaming
//! any of it is a breaking change:
//!
//! | | |
//! |---|---|
//! | target | `nitinol_eventsource::occ` |
//! | name | `non_genesis_sequence_conflict` |
//! | `aggregate_id` | the stream whose append was refused |
//! | `sequence` | the sequence it was refused at |
//!
//! It is emitted at `WARN`, so `RUST_LOG=nitinol_eventsource::occ=warn` selects
//! it and nothing else.
//!
//! Nothing else that ends an append emits it.  A refusal on the stream's
//! genesis sequence is a creation command arriving twice rather than a lost
//! race, and is answered as [`AskError::AlreadyCreated`]; a store that failed to
//! answer says nothing about who owns the stream; and a command the decider
//! refused never reached the store at all.
//!
//! What the event cannot say is *why* the stream was overtaken.  A second
//! activation of the same aggregate and an unrelated writer against the same
//! store produce the same refusal, so this is a count of lost races and not a
//! diagnosis of them.

mod appending;
pub mod codec;
mod durable_stream;
mod system_event;

pub mod error;
mod process;
pub mod projection;
pub mod system;

// Defined in `nitinol-contract` so a runtime-free domain crate can implement
// them; re-exported here because this crate's own API is stated in terms of
// them and because these are the paths downstream code already imports.
pub use nitinol_contract::{
    Accepting, Aggregate, Decider, Decision, Event, Latest, Never, Query, RecoverPolicy, Recovery,
    Snapshotable,
};

// Framework-managed persistent message abstraction. Hidden from docs and not
// re-exported through the umbrella crate so it stays an internal API; direct
// `nitinol-eventsource` importers can still reach it (consumer's responsibility).
#[doc(hidden)]
pub use self::error::SystemEventDecodeError;
#[doc(hidden)]
pub use self::system_event::SystemEvent;

// Matching a stored identity against the type that declared it, and building
// the row a user event is written down as.  Both are framework-internal: they
// are defined in terms of `Event` / `SystemEvent` rather than adding anything a
// domain has to declare.
#[doc(hidden)]
pub use self::appending::EventAppending;
#[doc(hidden)]
pub use nitinol_contract::{DeclaresEventType, MatchesEventType};

pub use self::error::{
    AskError, ExecError, MissingSnapshotSlot, Retryability, SnapshotSaveError, TellError,
};
pub use self::process::{
    AggregateProps, AggregateProxy, AggregateTellTarget, RecoverOn, RecoverOnContingent,
};
pub use self::process::{SnapshotPersistor, SnapshotPersistorProxy};

// The value an activation of a `Recover = Latest` aggregate carries. It appears
// in the signatures of `RecoverOn` and `RecoverOnContingent`, so it is public
// for the bounds to be nameable, and hidden because nothing outside this crate
// builds or reads one.
#[doc(hidden)]
pub use self::process::SnapshotRecovery;

#[cfg(feature = "test-helpers")]
pub mod test_helpers;

pub use self::projection::{
    EventEnvelope, EventSet, EventUnset, OriginSet, OriginUnset, ProjectionContext, ProjectionId,
    Projector, ProjectorProps, TxProvider,
};

pub use self::durable_stream::{
    CursorSet, CursorUnset, DurableStream, DurableStreamProxy, DurableSubscription, SequenceCursor,
    DEFAULT_BATCH_LIMIT, DEFAULT_POLL_INTERVAL,
};

// The message a projector's poller hands it and the answer it reads back. They
// appear in `ProjectorProcess`'s `Receive` impl, so they are public for that
// bound to be nameable, and hidden because nothing outside this crate sends one.
#[doc(hidden)]
pub use self::durable_stream::{PolledBatch, SettledThrough};
