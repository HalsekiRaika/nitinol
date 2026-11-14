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
/// event type, and each keeps its own offsets and its own consumed position.
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
    /// `key` is the stream key the events are appended under: an
    /// [`AggregateId`](nitinol_persistence::AggregateId) or another saga's
    /// [`SagaId`](crate::SagaId). The stream is read from the system's journal.
    /// The beginning is the floor; after a restart each partition resumes past
    /// the offset it recorded, counted in the stream's own sequence.
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
    /// For a source that must see several aggregates' events in the order the
    /// store accepted them. Its offset counts the global sequence; a record read
    /// again is still dropped by the per-source consumed positions before it
    /// reaches [`Saga::handle`](crate::Saga::handle).
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
    /// Where it resumes is the higher of this floor and the offset it
    /// recorded, so raising the floor above a recorded offset skips ahead,
    /// and lowering it below one changes nothing until the offset is lost.
    pub fn with_after(mut self, after: u64) -> Self {
        self.after = after;
        self
    }

    /// Ask this source's store no more often than every `interval`.
    ///
    /// Named per source, since the sources of one saga are read independently;
    /// a source that names none keeps the interval `nitinol-eventsource` polls
    /// at. What an interval costs is worked out on
    /// [`DEFAULT_POLL_INTERVAL`](nitinol_eventsource::DEFAULT_POLL_INTERVAL).
    /// The first read does not wait for it.
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
