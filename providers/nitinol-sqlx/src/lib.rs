//! A Postgres [`EventStore`] for `nitinol`, built on `sqlx`.
//!
//! [`EventStore`]: nitinol_persistence::store::EventStore
//!
//! # Getting one running
//!
//! ```rust,ignore
//! use std::sync::Arc;
//!
//! use nitinol_persistence::store::EventStore;
//! use nitinol_sqlx::{sqlx::postgres::PgPoolOptions, EventTypeRegistry, PostgresEventStore};
//!
//! # async fn wire(url: &str, my_event: nitinol_persistence::EventType) -> Result<(), Box<dyn std::error::Error>> {
//! let pool = PgPoolOptions::new().connect(url).await?;
//! PostgresEventStore::migrate(&pool).await?;
//!
//! let mut event_types = EventTypeRegistry::new();
//! event_types.register(my_event);
//!
//! let store: Arc<dyn EventStore> = Arc::new(PostgresEventStore::new(pool, event_types));
//! # Ok(())
//! # }
//! ```
//!
//! The pool is the caller's. The concurrency the store is judged on is the
//! concurrency the pool allows, so how many connections a deployment gives it —
//! and how it authenticates — stays a decision this crate does not make. `sqlx`
//! is re-exported so that pool is built against the same version this crate
//! links.
//!
//! # What a stream key has to be
//!
//! A stream key is unique across the whole store, not merely within a kind of
//! stream. Aggregates and sagas are journalled side by side in one table, keyed
//! by nothing but the string the caller appends under, so two of them sharing a
//! key share a stream: their sequence numbers collide and each refuses the
//! other's appends.
//!
//! # What a registry is for
//!
//! Every component of an [`EventType`](nitinol_persistence::EventType) is a
//! `&'static str`, so a path read out of a column cannot become one again
//! without leaking the string it was read into. [`EventTypeRegistry`] is the way
//! out: the caller declares the finite set of types the store may serve, and a
//! row carrying anything else fails its load rather than being passed over.

mod event_type_registry;
#[cfg(feature = "postgres")]
mod postgres;

pub use self::event_type_registry::{EventTypeRegistry, UnknownEventType};
#[cfg(feature = "postgres")]
pub use self::postgres::PostgresEventStore;

/// The `sqlx` this crate links, so a caller builds the pool it is handed
/// against the same version.
#[cfg(feature = "postgres")]
pub use sqlx;
