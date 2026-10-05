//! Persistence abstractions for the `nitinol` framework.
//!
//! This crate defines the storage traits consumed by `nitinol-eventsource`:
//!
//! | Trait / Type | Purpose |
//! |---|---|
//! | [`store::EventStore`] | Append and load domain events |
//! | [`store::SnapshotStore`] | Save and load aggregate snapshots |
//! | [`store::CheckpointStore`] | Track the progress of an event consumer |
//! | [`store::CheckpointScope`] | The key a consumer's progress is filed under |
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
//! [`store::EventStore`] carries an optimistic-concurrency-control contract
//! (`OCC-1` … `OCC-5`) that a backend has to reproduce, and one of its clauses
//! is broken silently by the most obvious way to number events in a relational
//! store.  `nitinol-store-conformance` is that contract as a suite to run
//! against your implementation.
//!
//! # Reserved namespace
//!
//! `nitinol` is reserved for the framework's own records, across **both** the
//! stream-key space and the event-type space — a single law with two
//! enforcement points, because the two spaces learn a name at different times.
//! An identifier inside it is refused when it is constructed; an event `family`
//! inside it is refused by [`Family::new`] while the constant holding it is
//! evaluated.  See [`reserved`] for the boundary rule, the constant, and the
//! way the framework's own records get the names they reserved.
//!
//! # Restoring an event type
//!
//! A stored row carries a canonical path, and every component of an
//! [`EventType`] is a `&'static str`, so the path cannot become one again
//! without leaking the string it was read into. [`EventRegistry`] is the way
//! back, and what it answers for is collected by the linker rather than
//! registered by hand: a declaration places an [`EventRegistryEntry`] in
//! [`EVENT_REGISTRY`], and the framework builds one registry per process out of
//! what the binary linked. There is nothing to construct and nothing to hand a
//! store. See [`EVENT_REGISTRY`] for what link-time collection does not reach.
//!
//! # One declaration per canonical path
//!
//! Two declarations claiming one path are refused twice over: each carries an
//! exported symbol named after its path, so the second is a definition rustc
//! refuses within a crate and the linker refuses across crates — and the
//! registry the process builds refuses the entries as well. The second layer
//! is not a spare: link-time detection has holes (static libraries linked
//! together need not report a duplicate), and an entry placed by hand rather
//! than derived carries no symbol at all.

pub mod error;
pub mod reserved;
pub mod store;

mod event;
mod event_registry;
mod event_symbol;
mod event_type;
mod id;
mod materialized_path;
mod query;
mod snapshot;

/// The way the framework's own records carry the names the framework reserved,
/// and nothing a user writes.
///
/// [`Family::new`] and [`EventRegistryEntry::new`] refuse the `nitinol`
/// namespace, which is the point of reserving it. The framework persists
/// records under exactly those names, so it needs constructors that waive the
/// namespace rule — and only that rule.
///
/// No stability is promised for anything below.
///
/// Building the registry is here for the same reason: what a store serves its
/// loads out of is the framework's own business, and the constructors below are
/// how the framework and the suites that judge its validation reach it.
#[doc(hidden)]
pub mod __private {
    pub use linkme;

    pub use crate::event_registry::{
        event_registry_from_entries, linked_event_registry, reserved_event_registry_entry,
    };
    pub use crate::event_symbol::{
        is_verbatim_event_symbol, is_verbatim_event_symbol_byte, EVENT_UNIQUENESS_SYMBOL_PREFIX,
    };
    pub use crate::event_type::reserved_family;
}

pub use event::{AppendingEvent, LoadedEvent};
pub use event_registry::{EventRegistry, EventRegistryEntry, EventRegistryError, EVENT_REGISTRY};
pub use event_type::{EventType, Family, ParsedEventType, TypeName};
pub use id::{AggregateId, ProjectionId};
pub use materialized_path::{MaterializedPath, MaterializedPathParseError};
pub use query::{AppendOutcome, LoadQuery};
pub use reserved::{is_within_reserved_namespace, reject_reserved_id, RESERVED_NAMESPACE};
pub use snapshot::PersistedSnapshot;
