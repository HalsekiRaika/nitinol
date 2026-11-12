//! The conditions a load is asked under, and the type its rows are read as.
//!
//! A store receives a [`LoadQuery<Untyped>`](LoadQuery) and serves stored rows.
//! A [`LoadQuery<Typed<E>>`](LoadQuery) is handed to a typed read instead, or
//! brought down to the untyped query a store receives with
//! [`erase`](LoadQuery::erase).

use std::borrow::Borrow;
use std::marker::PhantomData;

use nitinol_contract::{Event, EventType, MaterializedPath};

mod read_type;

pub use read_type::{Typed, Untyped};

/// The conditions a load selects rows under, and what it reads them as.
///
/// `T` is the read type. [`Untyped`], the default, is the query
/// [`EventStore::load`](crate::store::EventStore::load) receives: rows as they
/// are stored. [`Typed<E>`] is the query
/// [`TypedLoad::read`](crate::store::TypedLoad::read) receives: the rows of `E`
/// read back as `E`. It is built by [`LoadQuery::of`] alone, and
/// [`erase`](LoadQuery::erase) hands a store the same conditions with `E`'s
/// path as the prefix.
///
/// A query is built from the constructors and combinators below rather than
/// written out field by field: the read type is not a field a caller sets.
#[derive(Debug, Clone)]
pub struct LoadQuery<T = Untyped> {
    /// The stream to serve events from, or every stream if `None`.
    pub stream_key: Option<String>,
    /// The event type path the served events lie within, or every type if
    /// `None`.
    pub event_type_prefix: Option<MaterializedPath>,
    /// The lowest global position to serve, inclusive.
    pub from_global_sequence: Option<u64>,
    /// The lowest stream sequence to serve, inclusive.
    pub from_stream_sequence: Option<u64>,
    /// The most events to serve, or no bound if `None`.
    pub limit: Option<usize>,
    #[expect(
        dead_code,
        reason = "the read type is held for the type it names; no value is read out of it"
    )]
    read_as: T,
}

/// Written out for the untyped query alone rather than derived: a derive would
/// be generic over `T`, so `LoadQuery::default()` would no longer say which
/// query it builds, and a typed query is built by [`LoadQuery::of`] alone.
impl Default for LoadQuery {
    fn default() -> Self {
        Self {
            stream_key: None,
            event_type_prefix: None,
            from_global_sequence: None,
            from_stream_sequence: None,
            limit: None,
            read_as: Untyped,
        }
    }
}

impl<T> LoadQuery<T> {
    /// Adds the stream condition to this query, keeping the others it carries.
    ///
    /// Takes the stream key in any form [`by_stream`](LoadQuery::by_stream)
    /// takes.
    pub fn with_stream<K: ?Sized + Borrow<str>>(mut self, key: &K) -> Self {
        self.stream_key = Some(key.borrow().to_owned());
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

    /// Sets the most events this query serves, keeping the other conditions it
    /// carries.
    pub fn with_limit(mut self, n: usize) -> Self {
        self.limit = Some(n);
        self
    }
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

    /// Builds a query for the events whose type path lies within `prefix`.
    pub fn by_event_type_prefix(prefix: MaterializedPath) -> Self {
        Self {
            event_type_prefix: Some(prefix),
            ..Default::default()
        }
    }

    /// Builds a query for the events at or after global position `seq`.
    pub fn from_global(seq: u64) -> Self {
        Self {
            from_global_sequence: Some(seq),
            ..Default::default()
        }
    }

    /// Adds the event type prefix condition to this query, keeping the others
    /// it carries.
    pub fn with_event_type_prefix(mut self, prefix: MaterializedPath) -> Self {
        self.event_type_prefix = Some(prefix);
        self
    }

    /// The rows of `E`, read back as `E`, under no other condition yet.
    ///
    /// The query names `E`'s path as its prefix; the combinators every query
    /// has add the rest.
    pub fn of<E: Event>() -> LoadQuery<Typed<E>> {
        LoadQuery {
            stream_key: None,
            event_type_prefix: Some(E::EVENT_TYPE.to_path()),
            from_global_sequence: None,
            from_stream_sequence: None,
            limit: None,
            read_as: Typed(PhantomData),
        }
    }
}

impl<E: Event> LoadQuery<Typed<E>> {
    /// The query the store receives: the same conditions, with `E`'s path as
    /// the prefix.
    ///
    /// The prefix is `E`'s path whatever the prefix field was left holding, so
    /// what a store is asked for is always the rows `E` names.
    pub fn erase(self) -> LoadQuery {
        let LoadQuery {
            stream_key,
            event_type_prefix: _,
            from_global_sequence,
            from_stream_sequence,
            limit,
            read_as: _,
        } = self;
        LoadQuery {
            stream_key,
            event_type_prefix: Some(E::EVENT_TYPE.to_path()),
            from_global_sequence,
            from_stream_sequence,
            limit,
            read_as: Untyped,
        }
    }
}

/// What an append assigned.
#[derive(Debug, Clone)]
pub struct AppendOutcome {
    /// The global positions given to the appended events, in the order they
    /// were appended.
    pub assigned_sequences: Vec<u64>,
    /// The highest sequence among the appended events, or, for an empty batch,
    /// the sequence the stream had already reached.
    pub stream_version: u64,
}
