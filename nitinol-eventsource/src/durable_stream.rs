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
/// reads at once. A projector also skips the wait after a read that filled its
/// batch limit and settled whole, see [`DEFAULT_BATCH_LIMIT`].
///
/// What an interval buys and costs can be worked out before a deployment picks
/// one. The floor on latency is the sum of the intervals of the pollers an
/// event passes through: one interval on a path through a single poller, as a
/// projector's is, and two on a saga's round trip, where the subscription on
/// the observed source and then the delivery host each read it in turn. At the
/// default of 250 ms that round trip waits about 500 ms; at 1 ms it waits about
/// 3 ms, which is what `saga_round_trip/poll_1ms` in BENCHMARKS.md measures
/// (2.9 ms).
///
/// Each poller asks the store once every interval, so the load on the store is
/// the number of pollers divided by the interval: at 1 ms, 1,000 queries a
/// second for every poller.
///
/// What one of those queries costs is the store's. The in-memory store walks
/// the whole journal on every load, so what a shorter interval costs there
/// grows with the length of the journal; on Postgres one journal read took
/// about 0.45 ms (BENCHMARKS.md, the 2026-10-31 set).
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
