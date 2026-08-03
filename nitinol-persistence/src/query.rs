use std::borrow::Borrow;

use crate::event_type::EventType;
use crate::materialized_path::MaterializedPath;

#[derive(Debug, Clone, Default)]
pub struct LoadQuery {
    pub stream_key: Option<String>,
    pub event_type_prefix: Option<MaterializedPath>,
    pub from_global_sequence: Option<u64>,
    pub from_stream_sequence: Option<u64>,
    pub limit: Option<usize>,
}

impl LoadQuery {
    /// Build a query targeting a single stream.
    ///
    /// Accepts a reference to anything that `Borrow<str>`s — `AggregateId`,
    /// `SagaId`, `&str`, `String`, etc. — so the same store can serve
    /// heterogeneous stream-key newtypes side by side.  Mirrors the
    /// `HashMap::get<Q: ?Sized>(&Q) where K: Borrow<Q>` signature.
    pub fn by_stream<K: ?Sized + Borrow<str>>(key: &K) -> Self {
        Self {
            stream_key: Some(key.borrow().to_owned()),
            ..Default::default()
        }
    }

    /// Build a query targeting a specific [`EventType`].
    ///
    /// Uses the event type's Materialized Path as a prefix: a struct event
    /// (`variant = None`) matches exactly itself; an enum-arm event matches
    /// only that arm; a type-level query (`variant = None`) for a family that
    /// stores per-arm variants will match all arms under that type name, since
    /// the type path is an ancestor prefix of every arm path.
    pub fn by_event_type(et: EventType) -> Self {
        Self {
            event_type_prefix: Some(et.to_path()),
            ..Default::default()
        }
    }

    pub fn by_event_type_prefix(prefix: MaterializedPath) -> Self {
        Self {
            event_type_prefix: Some(prefix),
            ..Default::default()
        }
    }

    pub fn from_global(seq: u64) -> Self {
        Self {
            from_global_sequence: Some(seq),
            ..Default::default()
        }
    }

    pub fn with_limit(mut self, n: usize) -> Self {
        self.limit = Some(n);
        self
    }
}

#[derive(Debug, Clone)]
pub struct AppendOutcome {
    pub assigned_sequences: Vec<u64>,
    pub stream_version: u64,
}
