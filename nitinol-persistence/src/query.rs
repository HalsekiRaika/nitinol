use std::borrow::Borrow;

use nitinol_contract::{EventType, MaterializedPath};

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
    /// Uses the event type's Materialized Path as the prefix, so this selects
    /// the rows of that event type *and* the rows of any deeper declaration
    /// whose family begins with that whole path — `shop.order.Opened.Late` is
    /// within `shop.order.Opened`. A reader that wants the one declaration
    /// alone compares each row's `event_type` with it, which is what
    /// [`TypedLoad::read`](crate::store::TypedLoad::read) does.
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

    /// Adds the stream condition to this query, keeping the others it carries.
    ///
    /// Takes the stream key in any form [`by_stream`](Self::by_stream) takes.
    pub fn with_stream<K: ?Sized + Borrow<str>>(mut self, key: &K) -> Self {
        self.stream_key = Some(key.borrow().to_owned());
        self
    }

    /// Adds the event type prefix condition to this query, keeping the others
    /// it carries.
    pub fn with_event_type_prefix(mut self, prefix: MaterializedPath) -> Self {
        self.event_type_prefix = Some(prefix);
        self
    }

    /// Adds the global position condition to this query, keeping the others it
    /// carries. The position named is included.
    pub fn with_from_global(mut self, seq: u64) -> Self {
        self.from_global_sequence = Some(seq);
        self
    }

    /// Adds the stream sequence condition to this query, keeping the others it
    /// carries.
    pub fn with_from_stream_sequence(mut self, seq: u64) -> Self {
        self.from_stream_sequence = Some(seq);
        self
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
