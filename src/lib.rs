//! An event sourcing framework for Rust, built on a small process runtime.
//!
//! `nitinol` models a domain as event-sourced aggregates. You define an
//! aggregate's state, commands and events with the [`contract`] traits, and the
//! [`eventsource`] layer runs it: it persists the events each command produces
//! and rebuilds the state from them. Projectors turn the same events into read
//! models, and [`saga`]s coordinate work that spans aggregates and time. All of
//! them run as processes on the [`runtime`], which is built on Tokio.
//!
//! # Getting started
//!
//! Add `nitinol` with the features for the layers you use. The example below
//! also needs Tokio for its `main` and `bytes` for its codec:
//!
//! ```toml
//! [dependencies]
//! nitinol = { version = "0.6", features = ["eventsource"] }
//! tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
//! bytes = "1"
//! ```
//!
//! A counter that answers an `Increment` command by recording an `Incremented`
//! event and returning its new value. Every event names the codec that writes
//! it to the store; `Incremented` carries no data, so its codec writes no bytes.
//!
//! ```
//! use std::convert::Infallible;
//! use bytes::Bytes;
//! use nitinol::eventsource::codec::Codec;
//! use nitinol::eventsource::system::EventSourceSystem;
//! use nitinol::eventsource::{Aggregate, Decider, Decision, Event, Never};
//! use nitinol::persistence::{provider::PersistenceProvider, store::InMemoryStore, AggregateId};
//! use nitinol::runtime::ProcessSystem;
//!
//! #[derive(Clone, Event)]
//! #[event(family = "counter", codec = CounterCodec)]
//! struct Incremented;
//!
//! struct CounterCodec;
//!
//! impl Codec<Incremented> for CounterCodec {
//!     type Error = Infallible;
//!     fn encode(_event: &Incremented) -> Result<Bytes, Self::Error> {
//!         Ok(Bytes::new())
//!     }
//!
//!     fn decode(_payload: &[u8]) -> Result<Incremented, Self::Error> {
//!         Ok(Incremented)
//!     }
//! }
//!
//! #[derive(Default)]
//! struct Counter {
//!     value: u64,
//! }
//!
//! impl Aggregate for Counter {
//!     type Event = Incremented;
//!     type Recover = Never;
//!     fn apply(&mut self, _event: Incremented) {
//!         self.value += 1;
//!     }
//! }
//!
//! struct Increment;
//!
//! impl Decider<Increment> for Counter {
//!     type Output = u64;
//!     type Rejection = Infallible;
//!     fn decide(&self, _cmd: Increment) -> Decision<Incremented, u64, Infallible> {
//!         Decision::persist(vec![Incremented]).output(self.value + 1)
//!     }
//! }
//!
//! #[tokio::main]
//! async fn main() {
//!     let ps = ProcessSystem::new().await;
//!     let persistence = PersistenceProvider::new(InMemoryStore::default());
//!     let system = EventSourceSystem::builder(ps)
//!         .with_persistence(persistence)
//!         .build();
//!
//!     let counter = system
//!         .spawn_aggregate::<Counter>(AggregateId::new("counter-1"))
//!         .await;
//!     let value = counter.ask(Increment).await.expect("ask failed");
//!     assert_eq!(value, 1);
//! }
//! ```
//!
//! # Feature flags
//!
//! | Feature        | Contents |
//! |----------------|---------|
//! | `runtime`      | `runtime` — process runtime (`ProcessSystem`, `Process`, …) |
//! | `persistence`  | `persistence` — persistence abstractions (`EventStore`, IDs, …) |
//! | `contract`     | `contract` — runtime-free aggregate contract (`Aggregate`, `Decider`, `Decision`, `Query`, `Event`, `Codec`, `Snapshotable`, `Recovery` / `Never` / `Latest`, `#[derive(Event)]`) and the event identity it is written in (`EventType`, `Family`, `TypeName`, `MaterializedPath`, `EventRegistryEntry`, …) |
//! | `eventsource`  | `eventsource` — event sourcing layer (`Aggregate`, `Projector`, …) |
//! | `saga`         | `saga` — event-sourced process manager (`Saga`, `SagaEffect`, …) |
//! | `full`         | All of the above |
//! | `postgres`     | `sqlx` — Postgres backend (`PostgresEventStore`, `PostgresSnapshotStore`, `PostgresOffsetStore`) |
//!
//! `postgres` selects a database driver rather than a layer of the framework,
//! so `full` does not include it; enable it next to the layers you use.
//!
//! # Layers
//!
//! Each feature enables one module, and turns on the modules it builds on.
//!
//! [`runtime`] is the process runtime the other layers run on. A
//! [`ProcessSystem`](runtime::ProcessSystem) hosts processes that own their
//! state and talk by messages, under supervision. Implement
//! [`Process`](runtime::process::Process) and [`Receive`](runtime::process::Receive)
//! for a process of your own. See the [`runtime`] module for details.
//!
//! [`persistence`] keeps events, snapshots and consumer offsets. Store traits
//! such as [`EventStore`](persistence::store::EventStore) describe a backend, a
//! [`PersistenceProvider`](persistence::provider::PersistenceProvider) hands one
//! store to the whole system, and [`InMemoryStore`](persistence::store::InMemoryStore)
//! serves tests and examples. See the [`persistence`] module for details.
//!
//! [`contract`] holds what a domain layer implements to define an aggregate:
//! [`Aggregate`](contract::Aggregate), [`Decider`](contract::Decider) and
//! [`Event`](trait@contract::Event) with its derive. It pulls in no async
//! runtime, so use it from a domain crate that should not depend on Tokio. The
//! same items are also available under [`eventsource`]. See the [`contract`]
//! module for details.
//!
//! [`eventsource`] runs aggregates. An
//! [`EventSourceSystem`](eventsource::system::EventSourceSystem) spawns each
//! aggregate as a process, persists the events its decisions produce, and gives
//! back an [`AggregateProxy`](eventsource::AggregateProxy) to send it commands
//! and queries. A [`Projector`](eventsource::Projector) builds a read model from
//! stored events. See the [`eventsource`] module for details.
//!
//! [`saga`] coordinates work that spans aggregates and time. A
//! [`Saga`](saga::Saga) subscribes to upstream events, keeps its own
//! event-sourced state, and answers with [`SagaEffect`](saga::SagaEffect)s that
//! persist its decisions and set its deadlines; [`SagaSystemExt`](saga::SagaSystemExt)
//! hosts it on an `EventSourceSystem`. See the [`saga`] module for details.
//!
//! [`sqlx`], enabled by the `postgres` feature, is a Postgres backend for
//! [`persistence`]. Hand a [`PostgresStore`](sqlx::PostgresStore) to
//! `PersistenceProvider::new` to keep everything in one database, or use
//! [`PostgresEventStore`](sqlx::PostgresEventStore)
//! and its siblings one by one. See the [`sqlx`] module for details.
//!
//! # Examples
//!
//! Complete programs are in the repository, grouped by layer. Each is a
//! workspace package; run it from a clone with `cargo run -p <package>`.
//! `eventsource-basic-aggregate` is the full version of the counter above.
//!
//! - [`examples/runtime`](https://github.com/HalsekiRaika/nitinol/tree/master/examples/runtime):
//!   `basic-process`, `messaging`, `registry`, `watching`, `supervision`,
//!   `streams`, `dead-letters`
//! - [`examples/eventsource`](https://github.com/HalsekiRaika/nitinol/tree/master/examples/eventsource):
//!   `eventsource-basic-aggregate`, `eventsource-multiple-deciders`,
//!   `eventsource-projection`, `eventsource-snapshot`,
//!   `eventsource-aggregate-communication`
//! - [`examples/saga`](https://github.com/HalsekiRaika/nitinol/tree/master/examples/saga):
//!   `saga-basic-saga`, `saga-fanout-pattern`, `saga-idempotent-receive`

#![cfg_attr(docsrs, feature(doc_cfg))]
#![warn(missing_docs)]
// The links above name every layer's module, and a build without all of them
// has nothing to resolve the missing ones against.
#![cfg_attr(
    not(all(feature = "full", feature = "postgres")),
    allow(rustdoc::broken_intra_doc_links)
)]

#[cfg(feature = "runtime")]
#[cfg_attr(docsrs, doc(cfg(feature = "runtime")))]
pub use nitinol_runtime as runtime;

#[cfg(feature = "persistence")]
#[cfg_attr(docsrs, doc(cfg(feature = "persistence")))]
pub use nitinol_persistence as persistence;

/// Facade for the `contract` feature.
///
/// What a domain layer needs to define an aggregate, decide what its commands
/// mean, and answer queries about its state — the pure traits and the derive
/// that writes an `Event` impl — and nothing that runs one. This feature
/// reaches no async runtime, so a crate that keeps itself Tokio-free can
/// depend on `nitinol` through it alone.
///
/// The event identity a domain declares — its family, type name, canonical path
/// and registry entry, and the names the framework reserves — is part of the
/// contract and is named here too; `nitinol::persistence` re-exports the same
/// types as the shape of a stored row.
///
/// The `eventsource` feature re-exports these same trait items under
/// `nitinol::eventsource`; the two paths are interchangeable.
#[cfg(feature = "contract")]
#[cfg_attr(docsrs, doc(cfg(feature = "contract")))]
pub mod contract {
    pub use nitinol_contract::{
        is_within_reserved_namespace, Accepting, Aggregate, Codec, Decider, Decision, Event,
        EventRegistryEntry, EventType, Family, Latest, MaterializedPath,
        MaterializedPathParseError, Never, ParsedEventType, Query, RecoverPolicy, Recovery,
        Snapshotable, TypeName, RESERVED_NAMESPACE,
    };

    /// `#[derive(Event)]` macro, co-located with the `Event` trait so a single
    /// `use nitinol::contract::Event;` brings both into scope (trait in the
    /// type namespace, derive in the macro namespace — as `serde` does).
    pub use nitinol_macros::Event;
}

/// Facade for the `eventsource` feature.
///
/// Re-exports all user-facing APIs from `nitinol-eventsource`.
#[cfg(feature = "eventsource")]
#[cfg_attr(docsrs, doc(cfg(feature = "eventsource")))]
pub mod eventsource {
    pub use nitinol_eventsource::codec;

    /// User-facing error types from the eventsource layer.
    pub mod error {
        pub use nitinol_eventsource::error::AskError;
        pub use nitinol_eventsource::error::CodecError;
        pub use nitinol_eventsource::error::ExecError;
        pub use nitinol_eventsource::error::MissingSnapshotSlot;
        pub use nitinol_eventsource::error::PersistError;
        pub use nitinol_eventsource::error::Retryability;
        pub use nitinol_eventsource::error::SnapshotSaveError;
        pub use nitinol_eventsource::error::TellError;
    }

    pub use nitinol_eventsource::projection;
    pub use nitinol_eventsource::system;

    pub use nitinol_eventsource::Event;
    pub use nitinol_eventsource::{Accepting, Decider, Decision};
    pub use nitinol_eventsource::{Aggregate, Snapshotable};
    pub use nitinol_eventsource::{
        Latest, Never, RecoverOn, RecoverOnContingent, RecoverPolicy, Recovery,
    };

    /// `#[derive(Event)]` macro, co-located with the `Event` trait so a single
    /// `use nitinol::eventsource::Event;` brings both into scope (trait in the
    /// type namespace, derive in the macro namespace — as `serde` does).
    pub use nitinol_macros::Event;

    pub use nitinol_eventsource::Query;
    pub use nitinol_eventsource::{AggregateProps, AggregateProxy, AggregateTellTarget};
    pub use nitinol_eventsource::{
        AskError, ExecError, MissingSnapshotSlot, Retryability, SnapshotSaveError, TellError,
    };
    pub use nitinol_eventsource::{
        CursorSet, CursorUnset, DurableStream, DurableStreamProxy, DurableSubscription,
        SequenceCursor,
    };
    pub use nitinol_eventsource::{
        EventEnvelope, EventSet, EventUnset, OriginSet, OriginUnset, ProjectionContext,
        ProjectionId, Projector, ProjectorProps, TxProvider,
    };
    pub use nitinol_eventsource::{SnapshotPersistor, SnapshotPersistorProxy};
}

#[cfg(feature = "saga")]
#[cfg_attr(docsrs, doc(cfg(feature = "saga")))]
pub use nitinol_saga as saga;

#[cfg(feature = "postgres")]
#[cfg_attr(docsrs, doc(cfg(feature = "postgres")))]
pub use nitinol_sqlx as sqlx;

/// What the code `#[derive(Event)]` writes reaches, and nothing a user writes.
///
/// The expansion places an entry in
/// [`persistence::EVENT_REGISTRY`](nitinol_persistence::EVENT_REGISTRY), which
/// takes the machinery that fills that slice — and a user who derives `Event`
/// is obliged to depend on `nitinol` and on nothing else, so the derive reaches
/// it here rather than through a dependency the user would have to add.
///
/// No stability is promised for anything below.
#[cfg(feature = "contract")]
#[doc(hidden)]
pub mod __private {
    pub use linkme;
}
