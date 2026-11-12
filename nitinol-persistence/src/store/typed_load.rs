//! Reading the rows of one event type back as that type.

use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::task::{ready, Context, Poll};

use futures_core::Stream;
use jiff::Timestamp;
use nitinol_contract::{Codec, Event};

use crate::error::LoadError;
use crate::event::LoadedEvent;
use crate::query::{LoadQuery, Typed};
use crate::store::event_store::{EventStore, EventStream};

/// The rows a typed read serves, each read back as `E` or failed in its place.
pub type TypedEventStream<'a, E> =
    Pin<Box<dyn Stream<Item = Result<TypedEvent<E>, LoadError>> + Send + 'a>>;

/// A stored row of `E`, read back through `E::Codec`.
///
/// The row's identity and payload are replaced by the value they encode; where
/// the row sits — its stream, its place in that stream, its global position
/// and its instant — is kept, because a consumer that follows the journal
/// takes its progress from it.
#[derive(Debug, Clone)]
pub struct TypedEvent<E> {
    /// The stream the row was appended to.
    pub stream_key: String,
    /// The row's position in its stream.
    pub sequence: u64,
    /// The row's position across the store.
    pub global_sequence: u64,
    /// When the event occurred.
    pub occurred_at: Timestamp,
    /// The event the row's payload decodes to.
    pub event: E,
}

/// Reads the rows of one event type out of an event store, as that type.
///
/// nitinol has no search API: searching is the work of a projection's read
/// model. This reads the rows of a single type under the conditions the store
/// itself serves, once — it does not follow the journal as rows are appended.
///
/// Every [`EventStore`] has it, `dyn EventStore` included, and so do a
/// [`Journal`](crate::store::Journal) and the
/// [`InMemoryStore`](crate::store::InMemoryStore) that holds one: neither is an
/// event store, so each hands the read to the one it erases or holds. It is
/// derived from what a store already promises rather than something a backend
/// implements, and it cannot be implemented outside this crate:
///
/// ```compile_fail
/// use nitinol_persistence::store::TypedLoad;
///
/// struct Elsewhere;
///
/// impl TypedLoad for Elsewhere {
///     async fn read<E: nitinol_contract::Event>(
///         &self,
///         _: nitinol_persistence::LoadQuery<nitinol_persistence::Typed<E>>,
///     ) -> Result<nitinol_persistence::store::TypedEventStream<'_, E>, nitinol_persistence::error::LoadError> {
///         unimplemented!()
///     }
/// }
/// ```
///
/// The type a read decodes into is the one its query was built for, so a read
/// is handed a query from [`LoadQuery::of`]:
///
/// ```
/// # use nitinol_contract::{Event, EventType, Family, TypeName};
/// # use nitinol_persistence::store::{InMemoryEventStore, TypedLoad};
/// # use nitinol_persistence::LoadQuery;
/// # #[derive(Clone)]
/// # struct Opened;
/// # struct Nothing;
/// # impl nitinol_contract::Codec<Opened> for Nothing {
/// #     type Error = std::convert::Infallible;
/// #     fn encode(_: &Opened) -> Result<bytes::Bytes, Self::Error> { Ok(bytes::Bytes::new()) }
/// #     fn decode(_: &[u8]) -> Result<Opened, Self::Error> { Ok(Opened) }
/// # }
/// # impl Event for Opened {
/// #     const EVENT_TYPE: EventType = EventType::new(Family::new("shop.order"), TypeName::new("Opened"));
/// #     type Codec = Nothing;
/// # }
/// # async fn reads(store: &InMemoryEventStore) {
/// let opened = store.read(LoadQuery::of::<Opened>().with_stream("order-1")).await;
/// # }
/// ```
///
/// and a query built for no type is not one it reads:
///
/// ```compile_fail
/// # use nitinol_contract::{Event, EventType, Family, TypeName};
/// # use nitinol_persistence::store::{InMemoryEventStore, TypedLoad};
/// # use nitinol_persistence::LoadQuery;
/// # #[derive(Clone)]
/// # struct Opened;
/// # struct Nothing;
/// # impl nitinol_contract::Codec<Opened> for Nothing {
/// #     type Error = std::convert::Infallible;
/// #     fn encode(_: &Opened) -> Result<bytes::Bytes, Self::Error> { Ok(bytes::Bytes::new()) }
/// #     fn decode(_: &[u8]) -> Result<Opened, Self::Error> { Ok(Opened) }
/// # }
/// # impl Event for Opened {
/// #     const EVENT_TYPE: EventType = EventType::new(Family::new("shop.order"), TypeName::new("Opened"));
/// #     type Codec = Nothing;
/// # }
/// # async fn reads(store: &InMemoryEventStore) {
/// let opened = store.read(LoadQuery::by_stream("order-1")).await;
/// # }
/// ```
pub trait TypedLoad: sealed::Sealed {
    /// Serves the rows of `E` that `query` selects, each read through
    /// `E::Codec`.
    ///
    /// # LOAD-2: a typed read is a load of the type's own rows
    ///
    /// `read(q)` is what [`EventStore::load`] serves for
    /// [`q.erase()`](LoadQuery::erase), keeping the rows whose `event_type` is
    /// `E::EVENT_TYPE` and reading each through `E::Codec`, in the order and at
    /// the positions `load` serves them.
    ///
    /// It follows from `LOAD-1` and is not something a backend implements:
    ///
    /// - The type is part of the query, so the store selects it: the query the
    ///   store receives names `E`'s path as its prefix.
    /// - A prefix naming `E`'s path also reaches the rows of a deeper
    ///   declaration whose family is that whole path. Those rows are left out:
    ///   they are not `E`. A limit the query names counts them all the same,
    ///   since it is the store that applies it.
    /// - A payload `E::Codec` cannot read is a [`LoadError::Decode`] in that
    ///   row's place, and the rows after it are still read. Skipping it would
    ///   let a consumer that moves its progress past the rows it was served
    ///   lose the row without ever being told.
    ///
    /// Fixed by `nitinol-persistence/tests/integration/typed_load.rs` against
    /// the reference implementation.
    ///
    /// # Errors
    ///
    /// The outer `Err` is the error [`EventStore::load`] returned for
    /// [`query.erase()`](LoadQuery::erase).
    fn read<E: Event>(
        &self,
        query: LoadQuery<Typed<E>>,
    ) -> impl Future<Output = Result<TypedEventStream<'_, E>, LoadError>> + Send;
}

impl<S: EventStore + ?Sized> TypedLoad for S {
    async fn read<E: Event>(
        &self,
        query: LoadQuery<Typed<E>>,
    ) -> Result<TypedEventStream<'_, E>, LoadError> {
        Ok(Box::pin(TypedRows {
            rows: self.load(query.erase()).await?,
            event: PhantomData::<fn() -> E>,
        }))
    }
}

/// The rows a load served, read as `E` one at a time.
struct TypedRows<'a, E> {
    rows: EventStream<'a>,
    // A function pointer rather than `E` itself, so that whether the stream is
    // `Unpin` does not depend on `E`.
    event: PhantomData<fn() -> E>,
}

impl<E: Event> Stream for TypedRows<'_, E> {
    type Item = Result<TypedEvent<E>, LoadError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let rows = &mut self.get_mut().rows;
        loop {
            match ready!(rows.as_mut().poll_next(cx)) {
                None => return Poll::Ready(None),
                Some(Err(failure)) => return Poll::Ready(Some(Err(failure))),
                Some(Ok(row)) if row.event_type != E::EVENT_TYPE => {}
                Some(Ok(row)) => return Poll::Ready(Some(decoded(row))),
            }
        }
    }
}

fn decoded<E: Event>(row: LoadedEvent) -> Result<TypedEvent<E>, LoadError> {
    match <E::Codec as Codec<E>>::decode(&row.payload) {
        Ok(event) => Ok(TypedEvent {
            stream_key: row.stream_key,
            sequence: row.sequence,
            global_sequence: row.global_sequence,
            occurred_at: row.occurred_at,
            event,
        }),
        Err(source) => Err(LoadError::Decode {
            event_type: row.event_type,
            stream_key: row.stream_key,
            global_sequence: row.global_sequence,
            source: Box::new(source),
        }),
    }
}

mod sealed {
    use crate::store::event_store::EventStore;
    use crate::store::in_memory::InMemoryStore;
    use crate::store::journal::Journal;

    pub trait Sealed {}

    impl<S: EventStore + ?Sized> Sealed for S {}

    impl Sealed for Journal {}

    impl Sealed for InMemoryStore {}
}
