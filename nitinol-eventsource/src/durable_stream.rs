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
/// Override via `with_poll_interval` on either builder, or on
/// [`ProjectorProps`](crate::ProjectorProps) for a projector.
///
/// Public so a pull consumer outside this crate that leaves its cadence unset
/// — a saga's observed source, say — polls at this one rather than at a value
/// restated beside it.
///
/// The interval is the wait between one read and the next, not a wait before
/// the first: a poller reads as soon as it is spawned. A supervised restart
/// rebuilds a poller's state but keeps the driver that times it, so a poller
/// restarted after a failed read waits one interval before reading again; a
/// poller spawned afresh — by a projector or saga host being spawned, say —
/// reads at once.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(250);

pub use self::cursor::SequenceCursor;
pub use self::proxy::{DurableStreamProxy, DurableSubscription};
pub use self::stream::{CursorSet, CursorUnset, DurableStream};
