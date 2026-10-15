//! Where one declared source of a saga is read from.

use std::borrow::Borrow;
use std::time::Duration;

use nitinol_eventsource::SequenceCursor;
use nitinol_persistence::store::Journal;

use crate::subscribe::SourceBinding;

/// Which records one declared source is fed: the store to poll and the position
/// to start from, as one value.
///
/// The two travel together — a position means nothing without the store it
/// indexes into — so the spawn builder takes them as a single argument rather
/// than as a store plus a hand-built cursor.
///
/// One of these is handed to
/// [`observing`](crate::SagaManagerProps::observing) per declared source, so a
/// saga observing several sources names a store and a start position for each.
/// Two sources may name the same stream key: each reads only the rows of its own
/// event type, and each keeps its own cursor and its own consumed position.
///
/// A source reads either one stream ([`stream`](Self::stream)) or the whole store
/// in the order it accepted the records ([`global`](Self::global)).
pub struct Subscription {
    store: Journal,
    source: Source,
    after: u64,
    poll_interval: Option<Duration>,
}

/// Which records of the store the source reads: one stream, or all of them.
///
/// The two differ in the dimension `after` counts in — a stream's own sequence
/// against the store's global one — so they are one choice rather than a key
/// that may be absent.
enum Source {
    Stream { key: String },
    Global,
}

impl Subscription {
    /// Subscribe to one upstream stream, from its beginning.
    ///
    /// `key` is the stream key the upstream events are appended under — an
    /// [`AggregateId`](nitinol_persistence::AggregateId) for an aggregate's
    /// stream, a [`SagaId`](crate::SagaId) for another saga's own stream.
    /// Starting at the beginning means the manager sweeps everything already
    /// written before it was spawned, which is what activates the instances
    /// that are not resident; each instance drops the records its own stream
    /// names as consumed, so the sweep reaches [`Saga::handle`](crate::Saga::handle)
    /// only with records that one has not decided from.
    /// [`with_after`](Self::with_after) names a position to start past instead.
    ///
    /// The journal arrives as whatever the deployment holds: a backend handle,
    /// a [`Journal`] a supplier answered with, or a reference to one it keeps.
    pub fn stream<K>(store: impl Into<Journal>, key: &K) -> Self
    where
        K: ?Sized + Borrow<str>,
    {
        Self {
            store: store.into(),
            source: Source::Stream {
                key: key.borrow().to_owned(),
            },
            after: 0,
            poll_interval: None,
        }
    }

    /// Subscribe to every stream of one store at once, ordered by the store's
    /// global sequence.
    ///
    /// This is how a source observes several aggregates whose events must be
    /// seen in the order the store accepted them — an ordering a per-stream
    /// subscription cannot give, because each stream counts its own sequence.
    ///
    /// A store-wide position is not a stream position, so it is *not* carried
    /// forward from the consumed positions the saga's own stream records: those
    /// name a stream and its sequence, and reading one as a global sequence would
    /// skip records that were never consumed.  A source bound this way therefore
    /// starts where [`with_after`](Self::with_after) says and re-reads what it
    /// has already consumed after a restart — which the per-source consumed
    /// positions still drop before they reach
    /// [`Saga::handle`](crate::Saga::handle).
    /// The journal arrives in the same three forms
    /// [`stream`](Self::stream) accepts.
    pub fn global(store: impl Into<Journal>) -> Self {
        Self {
            store: store.into(),
            source: Source::Global,
            after: 0,
            poll_interval: None,
        }
    }

    /// Deliver only the records past sequence `after`, skipping the ones at or
    /// below it.
    ///
    /// A floor, not a starting point: a source bound this way never sees the
    /// records at or below it, however little of the stream has been consumed.
    pub fn with_after(mut self, after: u64) -> Self {
        self.after = after;
        self
    }

    /// Ask this source's store no more often than every `interval`.
    ///
    /// The observing side's counterpart of the delivery side's cadence —
    /// [`SagaManagerProps::with_delivery_poll_interval`](crate::SagaManagerProps::with_delivery_poll_interval)
    /// on a hosted saga's builder, or
    /// [`SagaDeliveryProps::with_poll_interval`](crate::SagaDeliveryProps::with_poll_interval)
    /// on a host whose stores are named by hand — and a deployment's choice for
    /// the same reason: a pull consumer trades
    /// latency for load on the store, and which way a deployment wants that is
    /// not something the framework can know.
    ///
    /// Named per source rather than per saga, because the sources of one saga
    /// are observed independently and need not be read at one cadence — a
    /// quiet stream polled rarely beside a busy one polled often. A source
    /// that names no interval keeps the one `nitinol-eventsource` already
    /// polls at.
    pub fn with_poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = Some(interval);
        self
    }

    pub(crate) fn into_binding(self) -> SourceBinding {
        let cursor = match self.source {
            Source::Stream { key } => SequenceCursor::Stream {
                key,
                after: self.after,
            },
            Source::Global => SequenceCursor::Global { after: self.after },
        };
        SourceBinding {
            upstream_store: self.store,
            cursor,
            poll_interval: self.poll_interval,
        }
    }
}
