//! A Postgres [`EventStore`] and [`OffsetStore`] for `nitinol`, built on
//! `sqlx`.
//!
//! [`EventStore`]: nitinol_persistence::store::EventStore
//! [`OffsetStore`]: nitinol_persistence::store::OffsetStore
//!
//! # Getting one running
//!
//! ```rust,ignore
//! use nitinol_persistence::provider::PersistenceProvider;
//! use nitinol_sqlx::{sqlx::postgres::PgPoolOptions, PostgresEventStore, PostgresStore};
//!
//! # async fn wire(url: &str) -> Result<(), Box<dyn std::error::Error>> {
//! let pool = PgPoolOptions::new().connect(url).await?;
//! PostgresEventStore::migrate(&pool).await?;
//!
//! // [`PostgresStore`] is this backend as one store: the journal and both
//! // offset slots over one database, and no snapshot slot.
//! let persistence = PersistenceProvider::new(PostgresStore::new(pool)?);
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
//! [`PostgresOffsetStore`] keeps a projection's offset in the same
//! database as the journal, and its `Tx` is a `sqlx` transaction, so the
//! read-model update and the offset that covers it can be one commit.
//! `PostgresEventStore::migrate` creates the table it uses.
//!
//! The transaction itself is the caller's. This crate defines no `TxProvider`:
//! that trait belongs to `nitinol-eventsource`, and a provider that depended on
//! the interpreter to implement it would invert the layering. Over a `PgPool`
//! it is `begin`, `commit` and `rollback` and nothing besides — see the crate
//! README for the implementation to copy, and for what a projection has to hold
//! to for `ExactlyOnce` to mean what it says.
//!
//! [`ProjectorProps::with_tx_provider`] requires the offset store's `Tx` to
//! be the provider's, so a pairing that would not share a transaction does not
//! compile.
//!
//! [`ProjectorProps::with_tx_provider`]: https://docs.rs/nitinol-eventsource
//!
//! ## An offset moves forward or not at all
//!
//! A save carrying a sequence the consumer has already reached, or one behind
//! it, is refused as [`OffsetConflict`] — never absorbed as a successful
//! no-op. Such a save means another writer holds that key, and under
//! `ExactlyOnce` the refusal is what rolls the read-model update back, so a
//! second writer becomes something someone can see rather than a counter
//! incremented twice. Refusing is this backend's answer to `OFS-4`, which
//! leaves the answer open; a consumer does not stop on it and saves again at
//! its next move forward.
//!
//! The refusal is worth telling apart from the other way a save fails. A
//! [`OffsetConflict`] says another writer holds this key, so retrying *this*
//! save reaches the same refusal; what moves the offset on is the consumer's
//! next move forward. Anything else — a dropped connection, a database that was
//! not there — arrives as the `sqlx` error that raised it, and a retry may well
//! clear it. Both reach a caller as [`OffsetError::Backend`], so which one it
//! is, is a `downcast_ref` away:
//!
//! ```rust,ignore
//! use nitinol_persistence::error::OffsetError;
//! use nitinol_sqlx::OffsetConflict;
//!
//! # fn classify(failure: &OffsetError) -> &'static str {
//! let OffsetError::Backend(cause) = failure;
//! if cause.downcast_ref::<OffsetConflict>().is_some() {
//!     "another writer holds this key; the consumer saves again when it moves on"
//! } else {
//!     "the database was not reachable; this one is worth retrying"
//! }
//! # }
//! ```
//!
//! [`OffsetError::Backend`]: nitinol_persistence::error::OffsetError::Backend
//!
//! ## What the rule does and does not guarantee
//!
//! It detects a duplicate and an offset going backwards. It is not a general
//! answer to running one projection from two places: a sequence genuinely past
//! the offset is taken from whoever saves it, and a projector holds the
//! offset it loaded at startup in memory. `ExactlyOnce` is guaranteed for a
//! single consumer per offset scope; the refusal above is what makes a second
//! writer visible, not a licence to run one.
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
    OffsetConflict, PostgresEventStore, PostgresOffsetStore, PostgresStore, UnknownEventType,
};

/// The `sqlx` this crate links, so a caller builds the pool it is handed
/// against the same version.
#[cfg(feature = "postgres")]
pub use sqlx;
