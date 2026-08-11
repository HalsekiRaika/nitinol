//! At-least-once event stream that catches up via [`EventStore`] polling and
//! then continues delivering live appends through the same channel.
//!
//! See [`DurableStream`] for the entry point.
//!
//! [`EventStore`]: nitinol_persistence::store::EventStore

use std::time::Duration;

mod cursor;
mod poller;
mod proxy;
mod stream;

/// Polling cadence shared by [`DurableStream`] and [`DurableSubscription`].
///
/// Chosen as a balance between catch-up latency and event-store load.
/// Override via `with_poll_interval` on either builder.
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(250);

pub use self::cursor::SequenceCursor;
pub use self::proxy::{DurableStreamProxy, DurableSubscription};
pub use self::stream::{CursorSet, CursorUnset, DurableStream};
