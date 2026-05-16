//! Umbrella crate for the `nitinol` event-sourcing framework.
//!
//! Re-exports sub-crates via optional features, Tokio-style.
//!
//! # Feature flags
//!
//! | Feature        | Contents |
//! |----------------|---------|
//! | `runtime`      | [`runtime`] — actor runtime (`ProcessSystem`, `Process`, …) |
//! | `persistence`  | [`persistence`] — persistence abstractions (`EventStore`, IDs, …) |
//! | `eventsource`  | [`eventsource`] — event sourcing layer (`Aggregate`, `Projector`, …) |
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

#[cfg(feature = "eventsource")]
pub use nitinol_eventsource as eventsource;
