//! Where one declared source of a saga is read from.

use std::borrow::Borrow;
use std::time::Duration;

use nitinol_eventsource::SequenceCursor;

use crate::subscribe::SourceBinding;

/// Which records one declared source is fed: the position to start from and the
/// cadence to ask at, as one value.
///
/// There is no store here. The journal every source is read from is the one the
/// system the saga is spawned on distributes, so a subscription names *what* to
/// observe and nothing about *where* — and a source in another destination is a
/// saga hosted on a second system over the supplier that holds it.
///
/// One of these is handed to
/// [`observing`](crate::SagaManagerProps::observing) per declared source, so a
/// saga observing several sources names a start position for each.
/// Two sources may name the same stream key: each reads only the rows of its own
/// event type, and each keeps its own cursor and its own consumed position.
///
/// A source reads either one stream ([`stream`](Self::stream)) or the whole
/// journal in the order it accepted the records ([`global`](Self::global)).
pub struct Subscription {
    source: Source,
    after: u64,
    poll_interval: Option<Duration>,
}

/// Which records of the journal the source reads: one stream, or all of them.
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
    /// The stream is read from the journal the system this saga is spawned on
    /// distributes. There is no form of this that names a store:
    ///
    /// ```compile_fail
    /// # use nitinol_persistence::store::InMemoryEventStore;
    /// # use nitinol_persistence::AggregateId;
    /// # use nitinol_saga::Subscription;
    /// # fn wiring() {
    /// let store = InMemoryEventStore::default();
    /// Subscription::stream(store, &AggregateId::new("order"));
    /// # }
    /// ```
    pub fn stream<K>(key: &K) -> Self
    where
        K: ?Sized + Borrow<str>,
    {
        Self {
            source: Source::Stream {
                key: key.borrow().to_owned(),
            },
            after: 0,
            poll_interval: None,
        }
    }

    /// Subscribe to every stream of the system's journal at once, ordered by
    /// the store's global sequence.
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
    ///
    /// The journal is the system's, the same one [`stream`](Self::stream) reads.
    pub fn global() -> Self {
        Self {
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
    /// [`SagaManagerProps::with_delivery_poll_interval`](crate::SagaManagerProps::with_delivery_poll_interval),
    /// whose contract is written on
    /// [`SagaDeliveryProps::with_poll_interval`](crate::SagaDeliveryProps::with_poll_interval)
    /// — and a deployment's choice for the same reason: a pull consumer trades
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
            cursor,
            poll_interval: self.poll_interval,
        }
    }
}
