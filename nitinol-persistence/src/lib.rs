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

pub mod error;
pub mod store;

mod event;
mod event_type;
mod id;
mod query;
mod snapshot;

pub use event::{AppendingEvent, LoadedEvent};
pub use event_type::{EventType, Family, TypeKey, TypeName, Variant};
pub use id::{AggregateId, ProjectionId};
pub use query::{AppendOutcome, LoadQuery};
pub use snapshot::PersistedSnapshot;
