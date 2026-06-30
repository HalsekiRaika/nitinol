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
        pub use nitinol_eventsource::error::TellError;
    }

    pub use nitinol_eventsource::projection;
    pub use nitinol_eventsource::system;

    pub use nitinol_eventsource::{Aggregate, Snapshotable};
    pub use nitinol_eventsource::Context;
    pub use nitinol_eventsource::Decider;
    pub use nitinol_eventsource::{Effect, SideEffect, SideEffectError};
    pub use nitinol_eventsource::Event;
    pub use nitinol_eventsource::Receive;
    pub use nitinol_eventsource::{AskError, ExecError, TellError};
    pub use nitinol_eventsource::{
        AggregateProps, AggregateProxy, AggregateTellTarget, CodecSet, CodecUnset,
    };
    pub use nitinol_eventsource::{SnapshotPersistor, SnapshotPersistorProxy};
    pub use nitinol_eventsource::{
        EventEnvelope, EventSet, EventUnset, OriginSet, OriginUnset, ProjectionContext, Projector,
        ProjectorProps, TxProvider,
    };
    pub use nitinol_eventsource::{
        CursorSet, CursorUnset, DurableStream, DurableStreamProxy, DurableSubscription,
        SequenceCursor,
    };
}

#[cfg(feature = "saga")]
pub use nitinol_saga as saga;
