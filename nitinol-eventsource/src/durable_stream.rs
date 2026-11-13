//! At-least-once event stream that catches up via [`EventStore`] polling and
//! then continues delivering live appends through the same channel.
//!
//! See [`DurableStream`] for the entry point.
//!
//! [`EventStore`]: nitinol_persistence::store::EventStore

use std::num::NonZeroUsize;
use std::time::Duration;

mod batch;
mod cursor;
mod poller;
mod proxy;
mod stream;

/// Polling cadence shared by [`DurableStream`] and [`DurableSubscription`]: 250 ms.
///
/// It is the wait between one read of the journal and the next (the first read
/// does not wait), so a shorter interval lowers latency and raises the load on
/// the store. Override it with `with_poll_interval` on either builder, or on
/// [`ProjectorProps`](crate::ProjectorProps) for a projector.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// How many records a projector reads from its journal at most at a time, when
/// its [`ProjectorProps`](crate::ProjectorProps) name no other limit.
///
/// What one read returns is the batch the projector settles as one unit: its
/// offset is saved once per batch, and under `ExactlyOnce` with a
/// [`TxProvider`](crate::TxProvider) the batch is one transaction. The batch is
/// held in memory while it is applied, so the limit bounds both the memory a
/// catch-up takes and the length of the transaction, whatever the size of the
/// backlog.
///
/// A read that came back with this many records and settled whole is followed by
/// the next one at once rather than an interval later, so a backlog is read back
/// to back; anything less waits the interval as usual.
pub const DEFAULT_BATCH_LIMIT: NonZeroUsize = NonZeroUsize::new(1_000).expect("1000 is not zero");

#[doc(hidden)]
pub use self::batch::{PolledBatch, SettledThrough};
pub use self::cursor::SequenceCursor;
pub use self::proxy::{DurableStreamProxy, DurableSubscription};
pub use self::stream::{CursorSet, CursorUnset, DurableStream};
