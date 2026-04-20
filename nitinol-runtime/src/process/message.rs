use std::any::Any;
use std::sync::Arc;

/// Marker trait for all types that can be published to a Stream.
///
/// Blanket-implemented for every `'static + Send + Sync` type so callers
/// never need to derive or implement it manually.
pub trait Message: 'static + Send + Sync {}

impl<T> Message for T where T: 'static + Send + Sync {}

/// Type-erased, cheaply-cloneable message container.
///
/// Backed by `Arc<dyn Any + Send + Sync>` so cloning is a reference-count
/// increment rather than a deep copy — ideal for broadcast delivery.
#[derive(Clone)]
pub struct Boxed(Arc<dyn Any + Send + Sync>);

impl Boxed {
    /// Wrap `value` in a `Boxed` container.
    pub fn new<T: Message>(value: T) -> Self {
        Self(Arc::new(value))
    }

    /// Attempt to retrieve a reference to the inner value as type `T`.
    ///
    /// Returns `None` if the inner type is not `T`.
    pub fn downcast_ref<T: Message>(&self) -> Option<&T> {
        self.0.downcast_ref::<T>()
    }
}
