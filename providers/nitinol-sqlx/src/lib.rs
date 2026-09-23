//! A Postgres [`EventStore`] and [`CheckpointStore`] for `nitinol`, built on
//! `sqlx`.
//!
//! [`EventStore`]: nitinol_persistence::store::EventStore
//! [`CheckpointStore`]: nitinol_persistence::store::CheckpointStore
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
//! # Projecting with `ExactlyOnce`
//!
//! [`PostgresCheckpointStore`] keeps a projection's checkpoint in the same
//! database as the journal, and its `Tx` is a `sqlx` transaction, so the
//! read-model update and the checkpoint that covers it can be one commit.
//! `PostgresEventStore::migrate` creates the table it uses.
//!
//! The transaction itself is the caller's. This crate defines no `TxProvider`:
//! that trait belongs to `nitinol-eventsource`, and a provider that depended on
//! the interpreter to implement it would invert the layering. Over a `PgPool`
//! it is `begin`, `commit` and `rollback` and nothing besides — see the crate
//! README for the implementation to copy, and for what a projection has to hold
//! to for `ExactlyOnce` to mean what it says.
//!
//! [`ProjectorProps::with_tx_provider`] requires the checkpoint store's `Tx` to
//! be the provider's, so a pairing that would not share a transaction does not
//! compile.
//!
//! [`ProjectorProps::with_tx_provider`]: https://docs.rs/nitinol-eventsource
//!
//! ## A checkpoint moves forward or not at all
//!
//! A save carrying a sequence the projection has already reached, or one behind
//! it, is refused as [`CheckpointConflict`] — never absorbed as a successful
//! no-op. Under `ExactlyOnce` that refusal is what rolls the read-model update
//! back, so a second writer on a projection meant to have one consumer becomes
//! an error someone can see rather than a counter incremented twice.
//!
//! The refusal is worth telling apart from the other way a save fails.
//! [`CheckpointConflict`] is a broken premise: retrying reaches the same
//! refusal, and what has to change is the deployment. Anything else — a dropped
//! connection, a database that was not there — arrives as the `sqlx` error that
//! raised it, and a retry may well clear it. Both reach a caller as
//! [`CheckpointError::Backend`], so which one it is, is a `downcast_ref` away:
//!
//! ```rust,ignore
//! use nitinol_persistence::error::CheckpointError;
//! use nitinol_sqlx::CheckpointConflict;
//!
//! # fn classify(failure: &CheckpointError) -> &'static str {
//! let CheckpointError::Backend(cause) = failure;
//! if cause.downcast_ref::<CheckpointConflict>().is_some() {
//!     "another writer holds this projection; retrying will not help"
//! } else {
//!     "the database was not reachable; this one is worth retrying"
//! }
//! # }
//! ```
//!
//! [`CheckpointError::Backend`]: nitinol_persistence::error::CheckpointError::Backend
//!
//! ## What the rule does and does not guarantee
//!
//! It detects a duplicate and a checkpoint going backwards. It is not a general
//! answer to running one projection from two places: a sequence genuinely past
//! the checkpoint is taken from whoever saves it, and a projector holds the
//! checkpoint it loaded at startup in memory. `ExactlyOnce` is guaranteed for a
//! single consumer per `projection_id`; the refusal above is the defence that
//! makes a broken premise visible, not a licence to break it.
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
pub use self::postgres::{
    CheckpointConflict, PostgresCheckpointStore, PostgresEventStore, UnknownEventType,
};

/// The `sqlx` this crate links, so a caller builds the pool it is handed
/// against the same version.
#[cfg(feature = "postgres")]
pub use sqlx;
