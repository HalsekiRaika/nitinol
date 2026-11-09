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
use crate::query::LoadQuery;
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
    pub stream_key: String,
    pub sequence: u64,
    pub global_sequence: u64,
    pub occurred_at: Timestamp,
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
///         _: nitinol_persistence::LoadQuery,
///     ) -> Result<nitinol_persistence::store::TypedEventStream<'_, E>, nitinol_persistence::error::LoadError> {
///         unimplemented!()
///     }
/// }
/// ```
pub trait TypedLoad: sealed::Sealed {
    /// Serves the rows of `E` that `query` selects, each read through
    /// `E::Codec`.
    ///
    /// # LOAD-2: a typed read is a load of the type's own rows
    ///
    /// `read::<E>(q)` is what [`EventStore::load`] serves for `q` with `E`'s
    /// path conjoined, keeping the rows whose `event_type` is `E::EVENT_TYPE`
    /// and reading each through `E::Codec`, in the order and at the positions
    /// `load` serves them.
    ///
    /// It follows from `LOAD-1` and is not something a backend implements:
    ///
    /// - The type is a condition of the query, so the store selects it. A query
    ///   naming no prefix, or a prefix `E`'s path is within, is narrowed to
    ///   `E`'s path; a query naming a prefix `E`'s path is not within names no
    ///   row of `E`, so the read serves nothing, and that is not an error.
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
    fn read<E: Event>(
        &self,
        query: LoadQuery,
    ) -> impl Future<Output = Result<TypedEventStream<'_, E>, LoadError>> + Send;
}

impl<S: EventStore + ?Sized> TypedLoad for S {
    async fn read<E: Event>(&self, query: LoadQuery) -> Result<TypedEventStream<'_, E>, LoadError> {
        let rows = match narrowed::<E>(query) {
            Some(query) => Some(self.load(query).await?),
            None => None,
        };
        Ok(Box::pin(TypedRows {
            rows,
            event: PhantomData::<fn() -> E>,
        }))
    }
}

/// `query` with `E`'s path conjoined, or `None` when the query names a prefix
/// `E`'s path is not within and so selects no row of `E`.
fn narrowed<E: Event>(query: LoadQuery) -> Option<LoadQuery> {
    let path = E::EVENT_TYPE.to_path();
    match &query.event_type_prefix {
        Some(prefix) if !path.is_within(prefix) => None,
        _ => Some(query.with_event_type_prefix(path)),
    }
}

/// The rows a load served, read as `E` one at a time; `None` serves nothing.
struct TypedRows<'a, E> {
    rows: Option<EventStream<'a>>,
    // A function pointer rather than `E` itself, so that whether the stream is
    // `Unpin` does not depend on `E`.
    event: PhantomData<fn() -> E>,
}

impl<E: Event> Stream for TypedRows<'_, E> {
    type Item = Result<TypedEvent<E>, LoadError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let Some(rows) = self.get_mut().rows.as_mut() else {
            return Poll::Ready(None);
        };
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
