//! Umbrella crate for the `nitinol` event-sourcing framework.
//!
//! Re-exports sub-crates via optional features, Tokio-style.
//!
//! # Feature flags
//!
//! | Feature        | Contents |
//! |----------------|---------|
//! | `runtime`      | `runtime` — actor runtime (`ProcessSystem`, `Process`, …) |
//! | `persistence`  | `persistence` — persistence abstractions (`EventStore`, IDs, …) |
//! | `contract`     | `contract` — runtime-free aggregate contract (`Aggregate`, `Event`, `Snapshotable`, `#[derive(Event)]`) |
//! | `eventsource`  | `eventsource` — event sourcing layer (`Aggregate`, `Projector`, …) |
//! | `saga`         | `saga` — event-sourced process manager (`Saga`, `SagaEffect`, …) |
//! | `full`         | All of the above |
//!
//! # Example
//!
//! ```toml
//! [dependencies]
//! nitinol = { version = "0.4", features = ["eventsource"] }
//! ```

#[cfg(feature = "runtime")]
pub use nitinol_runtime as runtime;

#[cfg(feature = "persistence")]
pub use nitinol_persistence as persistence;

/// Facade for the `contract` feature.
///
/// What a domain layer needs to define an aggregate — the three pure traits and
/// the derive that writes an `Event` impl — and nothing that runs one. This
/// feature reaches no async runtime, so a crate that keeps itself Tokio-free
/// can depend on `nitinol` through it alone.
///
/// The `eventsource` feature re-exports these same trait items under
/// `nitinol::eventsource`; the two paths are interchangeable. (That module is
/// not linked here because it does not exist in a `contract`-only build.)
#[cfg(feature = "contract")]
pub mod contract {
    pub use nitinol_contract::{Aggregate, Event, Snapshotable};

    /// `#[derive(Event)]` macro, co-located with the `Event` trait so a single
    /// `use nitinol::contract::Event;` brings both into scope (trait in the
    /// type namespace, derive in the macro namespace — as `serde` does).
    pub use nitinol_macros::Event;
}

/// Facade for the `eventsource` feature.
///
/// Re-exports all user-facing APIs from `nitinol-eventsource`.
/// Framework-internal types (`SystemEvent`, `appending_system_event`,
/// `SystemEventDecodeError`) are intentionally excluded so they are not
/// discoverable through the umbrella entry point.
/// Direct consumers of `nitinol-eventsource` can still reach them.
///
/// # Visibility contract — `error` sub-module
///
/// `SystemEventDecodeError` is a framework-internal type and is excluded from
/// the `error` facade below. Attempting to import it via the umbrella must fail
/// at compile time:
///
/// ```compile_fail
/// // SystemEventDecodeError is framework-internal; umbrella must not expose it.
/// use nitinol::eventsource::error::SystemEventDecodeError;
/// ```
#[cfg(feature = "eventsource")]
pub mod eventsource {
    pub use nitinol_eventsource::codec;

    /// User-facing error types from the eventsource layer.
    ///
    /// Framework-internal errors (`SystemEventDecodeError`, `AskHandlerError`,
    /// `ExecHandlerError`) are intentionally excluded — they form part of the
    /// internal request/response plumbing and are not part of the public API.
    pub mod error {
        pub use nitinol_eventsource::error::AskError;
        pub use nitinol_eventsource::error::CodecError;
        pub use nitinol_eventsource::error::EffectExecutionError;
        pub use nitinol_eventsource::error::ExecError;
        pub use nitinol_eventsource::error::Retryability;
        pub use nitinol_eventsource::error::TellError;
    }

    pub use nitinol_eventsource::projection;
    pub use nitinol_eventsource::system;

    pub use nitinol_eventsource::Context;
    pub use nitinol_eventsource::Decider;
    pub use nitinol_eventsource::Event;
    pub use nitinol_eventsource::{Aggregate, Snapshotable};
    pub use nitinol_eventsource::{Effect, SideEffect, SideEffectError};

    /// `#[derive(Event)]` macro, co-located with the `Event` trait so a single
    /// `use nitinol::eventsource::Event;` brings both into scope (trait in the
    /// type namespace, derive in the macro namespace — as `serde` does).
    pub use nitinol_macros::Event;

    pub use nitinol_eventsource::Query;
    pub use nitinol_eventsource::{
        AggregateProps, AggregateProxy, AggregateTellTarget, CodecSet, CodecUnset,
    };
    pub use nitinol_eventsource::{AskError, ExecError, Retryability, TellError};
    pub use nitinol_eventsource::{
        CursorSet, CursorUnset, DurableStream, DurableStreamProxy, DurableSubscription,
        SequenceCursor,
    };
    pub use nitinol_eventsource::{
        EventEnvelope, EventSet, EventUnset, OriginSet, OriginUnset, ProjectionContext, Projector,
        ProjectorProps, TxProvider,
    };
    pub use nitinol_eventsource::{SnapshotPersistor, SnapshotPersistorProxy};
}

#[cfg(feature = "saga")]
pub use nitinol_saga as saga;
