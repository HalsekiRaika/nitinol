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
//! use nitinol_sqlx::{sqlx::postgres::PgPoolOptions, PostgresEventStore};
//!
//! # async fn wire(url: &str) -> Result<(), Box<dyn std::error::Error>> {
//! let pool = PgPoolOptions::new().connect(url).await?;
//! PostgresEventStore::migrate(&pool).await?;
//!
//! let store: Arc<dyn EventStore> = Arc::new(PostgresEventStore::new(pool)?);
//! # Ok(())
//! # }
//! ```
//!
//! Nothing is registered: the event types the store serves are the ones this
//! binary linked. `#[derive(Event)]` writes that declaration for a domain type,
//! and the framework writes it for the records it journals itself.
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
//! # What a collected event type is for
//!
//! Every component of an [`EventType`](nitinol_persistence::EventType) is a
//! `&'static str`, so a path read out of a column cannot become one again
//! without leaking the string it was read into.
//! [`EventRegistry`](nitinol_persistence::EventRegistry) is the way out, and
//! what it answers for is gathered by the linker out of
//! [`EVENT_REGISTRY`](nitinol_persistence::EVENT_REGISTRY). A row carrying a
//! path nothing collected fails its load as [`UnknownEventType`] rather than
//! being passed over — see `EVENT_REGISTRY` for the one way collection comes up
//! short, a crate dropped from the link.

#[cfg(feature = "postgres")]
mod postgres;

#[cfg(feature = "postgres")]
pub use self::postgres::{PostgresEventStore, UnknownEventType};

/// The `sqlx` this crate links, so a caller builds the pool it is handed
/// against the same version.
#[cfg(feature = "postgres")]
pub use sqlx;
