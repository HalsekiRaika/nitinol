use std::any::type_name;
use std::fmt;
use std::marker::PhantomData;

/// A query that reads rows as they are stored: identity and payload, for
/// [`EventStore::load`](crate::store::EventStore::load).
///
/// This is what a store receives. A [`LoadQuery`](crate::LoadQuery) names it by
/// default, so `LoadQuery` on its own is `LoadQuery<Untyped>`.
#[derive(Debug, Clone, Copy, Default)]
pub struct Untyped;

/// A query that reads the rows of `E` back as `E`, for
/// [`TypedLoad::read`](crate::store::TypedLoad::read).
///
/// [`LoadQuery::of`](crate::LoadQuery::of) is the only way to one, so the type a
/// read decodes into and the path the store selects are named once, by `E`.
/// It is handed to `read`, or brought down to the [`Untyped`] query a store
/// receives with [`LoadQuery::into_untyped`](crate::LoadQuery::into_untyped).
pub struct Typed<E>(pub(super) PhantomData<fn() -> E>);

// Written out rather than derived: a derive would require `E` to be `Clone`,
// `Copy` and `Debug` as well, and the marker carries no `E` to clone or show.
impl<E> Clone for Typed<E> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<E> Copy for Typed<E> {}

impl<E> fmt::Debug for Typed<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Typed<{}>", type_name::<E>())
    }
}
