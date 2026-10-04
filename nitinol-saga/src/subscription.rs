//! Where one declared source of a saga is read from.

use std::borrow::Borrow;
use std::sync::Arc;

use nitinol_eventsource::SequenceCursor;
use nitinol_persistence::store::EventStore;

/// Which records one declared source is fed: the store to poll and the position
/// to start from, as one value.
///
/// The two travel together — a position means nothing without the store it
/// indexes into — so the spawn builders take them as a single argument rather
/// than as a store plus a hand-built cursor.
///
/// One of these is handed to
/// [`subscribed_to`](crate::SagaProps::subscribed_to) per declared source, so a
/// saga observing several sources names a store and a start position for each.
/// Two sources may name the same stream key: each reads only the rows of its own
/// event type, and each keeps its own cursor and its own consumed position.
///
/// A source reads either one stream ([`stream`](Self::stream)) or the whole store
/// in the order it accepted the records ([`global`](Self::global)).
pub struct Subscription {
    store: Arc<dyn EventStore>,
    source: Source,
    after: u64,
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
    /// Starting at the beginning means the saga catches up on everything already
    /// written before it was spawned — everything it has not already consumed,
    /// that is: a restarted saga resumes past the positions its own stream
    /// records for this source, so "from the beginning" costs a re-read of the
    /// upstream history only on a source nothing has been consumed from.
    /// [`with_after`](Self::with_after) names a position to start past instead.
    pub fn stream<K>(store: &Arc<dyn EventStore>, key: &K) -> Self
    where
        K: ?Sized + Borrow<str>,
    {
        Self {
            store: Arc::clone(store),
            source: Source::Stream {
                key: key.borrow().to_owned(),
            },
            after: 0,
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
    pub fn global(store: &Arc<dyn EventStore>) -> Self {
        Self {
            store: Arc::clone(store),
            source: Source::Global,
            after: 0,
        }
    }

    /// Deliver only the records past sequence `after`, skipping the ones at or
    /// below it.
    ///
    /// A floor, not a starting point: a source that has consumed further already
    /// resumes from its own recorded position, and one that has consumed less
    /// still never sees the records below this one.
    pub fn with_after(mut self, after: u64) -> Self {
        self.after = after;
        self
    }

    pub(crate) fn into_parts(self) -> (Arc<dyn EventStore>, SequenceCursor) {
        let cursor = match self.source {
            Source::Stream { key } => SequenceCursor::Stream {
                key,
                after: self.after,
            },
            Source::Global => SequenceCursor::Global { after: self.after },
        };
        (self.store, cursor)
    }
}
