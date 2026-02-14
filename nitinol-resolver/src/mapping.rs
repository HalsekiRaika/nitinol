#[cfg(feature = "process")]
pub mod process;

use std::collections::HashMap;
use std::sync::Arc;

use nitinol_core::event::Event;

use crate::resolver::{ResolveHandler, ResolveType, Resolver, ResolverType, TypedResolver};

/// ResolveMapping is a trait that defines the mapping of event types to their respective handlers.
///
/// ## Example
/// ```rust
/// # use async_trait::async_trait;
/// # use serde::{Deserialize, Serialize};
/// # use nitinol_core::errors::{DeserializeError, SerializeError};
/// # use nitinol_core::event::Event;
/// # use nitinol_resolver::resolver::ResolveHandler;
/// #
/// # pub struct Entity;
/// #
/// # #[derive(Debug, Deserialize, Serialize)]
/// # pub enum EntityEvent {}
/// #
/// # impl Event for EntityEvent {
/// #     const EVENT_TYPE: &'static str = "entity-event";
/// #
/// #     fn as_bytes(&self) -> Result<Vec<u8>, SerializeError> {
/// #         Ok(serde_json::to_vec(self)?)
/// #     }
/// #     fn from_bytes(bytes: &[u8]) -> Result<Self, DeserializeError> {
/// #         Ok(serde_json::from_slice(bytes)?)
/// #     }
/// # }
/// #
/// # pub struct Subscribe;
/// #
/// # #[async_trait]
/// # pub trait SubscribeHandler<E: Event>: 'static + Sync + Send + Sized {
/// #     type Rejection: std::fmt::Debug + Sync + Send + 'static;
/// #     async fn on(&mut self, event: E) -> Result<(), Self::Rejection>;
/// # }
/// #
/// # #[async_trait]
/// # impl<E: Event, T> ResolveHandler<E, T> for Subscribe
/// # where
/// #     T: SubscribeHandler<E>,
/// # {
/// #     const HANDLER_TYPE: &'static str = "subscribe";
/// #     type Error = T::Rejection;
/// #
/// #     async fn apply(entity: &mut Option<T>, event: E) -> Result<(), Self::Error> {
/// #         let Some(entity) = entity else {
/// #             panic!("Entity must exist in this process.");
/// #         };
/// #
/// #         entity.on(event).await?;
/// #
/// #         Ok(())
/// #     }
/// # }
/// #
/// use nitinol_resolver::mapping::{Mapper, ResolveMapping};
///
/// #[async_trait]
/// impl SubscribeHandler<EntityEvent> for Entity {
///     type Rejection = String;
///     async fn on(&mut self, event: EntityEvent) -> Result<(), Self::Rejection> {
///         // something process...
///         Ok(())
///     }
/// }
///
/// impl ResolveMapping for Entity {
///     fn mapping(mapper: &mut Mapper<Self>) {
///         // Register the event type and its handler
///         // This `Subscribe` shown as an example points out a compile-error,
///         // if the above `SubscribeHandler` is not implemented for the Entity type.
///         mapper.register::<EntityEvent, Subscribe>();
///     }
/// }
/// ```
pub trait ResolveMapping: 'static + Sync + Send + Sized {
    fn mapping(mapper: &mut Mapper<Self>);
}

pub struct Mapper<T> {
    map: HashMap<ResolveType, Arc<dyn Resolver<T>>>,
}

impl<T> Mapper<T> 
where
    T: 'static + Sync + Send,
{
    pub fn register<E: Event, H>(&mut self) -> &mut Self
    where
        H: ResolveHandler<E, T>,
    {
        self.map.insert(
            ResolveType::new(E::EVENT_TYPE, H::HANDLER_TYPE),
            Arc::new(TypedResolver::<E, T, H>::default()),
        );
        self
    }
    
    pub fn register_with<E: Event, R>(&mut self, resolver: R) -> &mut Self 
    where
        R: ResolverType<T>
    {
        self.map.insert(
            ResolveType::new(E::EVENT_TYPE, R::RESOLVE_TYPE),
            Arc::new(resolver),
        );
        self
    }

    pub fn find(&self, mut f: impl FnMut(&ResolveType) -> bool) -> Option<Arc<dyn Resolver<T>>> {
        self.map
            .iter()
            .find(|(key, _)| f(key))
            .map(|(_, handler)| handler)
            .cloned()
    }
    
    pub fn filter(self, mut f: impl FnMut(&ResolveType) -> bool) -> Self {
        Self {
            map: self.map.into_iter()
                .filter(|(key, _)| f(key))
                .collect(),
        }
    }
}

impl<T> Default for Mapper<T> {
    fn default() -> Self {
        Self {
            map: HashMap::default(),
        }
    }
}
