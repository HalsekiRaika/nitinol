//! Persistence abstractions for the `nitinol` framework.
//!
//! This crate defines the storage traits consumed by `nitinol-eventsource`:
//!
//! | Trait / Type | Purpose |
//! |---|---|
//! | [`store::EventStore`] | Append and load domain events |
//! | [`store::SnapshotStore`] | Save and load aggregate snapshots |
//! | [`store::CheckpointStore`] | Track the delivery progress of a projector |
//! | [`store::DeliveryMode`] | Delivery guarantee: `AtMostOnce` / `AtLeastOnce` / `ExactlyOnce` |
//!
//! An in-memory reference implementation is provided under [`store`] for
//! use in tests and examples.  It is **not** intended for production use.
//!
//! # Third-party backends
//!
//! Production backends (Postgres, SQLite, DynamoDB, etc.) are intentionally
//! out of scope for this crate.  Implement [`store::EventStore`],
//! [`store::SnapshotStore`], and [`store::CheckpointStore`] against your
//! preferred storage engine and wire them through
//! `nitinol_eventsource::system::ProcessSystem`.
//!
//! # Reserved namespace
//!
//! `nitinol` is reserved for the framework's own records, across **both** the
//! stream-key space and the event-type space — a single law with two
//! enforcement points, because the two spaces learn a name at different times.
//! An identifier inside it is refused when it is constructed; an event `family`
//! inside it is refused by `#[derive(Event)]` when it is expanded.  See
//! [`reserved`] for the boundary rule, the constant, and what a hand-written
//! `impl Event` is expected to honour on its own.

pub mod error;
pub mod reserved;
pub mod store;

mod event;
mod event_type;
mod id;
mod materialized_path;
mod query;
mod snapshot;

pub use event::{AppendingEvent, LoadedEvent};
pub use event_type::{EventType, Family, ParsedEventType, TypeKey, TypeName, Variant};
pub use id::{AggregateId, ProjectionId};
pub use materialized_path::{MaterializedPath, MaterializedPathParseError};
pub use query::{AppendOutcome, LoadQuery};
pub use reserved::{is_within_reserved_namespace, reject_reserved_id, RESERVED_NAMESPACE};
pub use snapshot::PersistedSnapshot;
