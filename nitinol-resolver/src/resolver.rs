use std::fmt::{Debug, Display};
use std::marker::PhantomData;

use async_trait::async_trait;
use nitinol_core::event::Event;

use crate::errors::ResolveError;
use crate::mapping::ResolveMapping;

#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub struct ResolveType {
    event: &'static str,
    handler: &'static str,
}

impl ResolveType {
    pub const fn new(event: &'static str, handler: &'static str) -> Self {
        Self { event, handler }
    }

    pub fn event(&self) -> &'static str {
        self.event
    }

    pub fn handler(&self) -> &'static str {
        self.handler
    }
}

impl Display for ResolveType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}-{}", self.event, self.handler)
    }
}

#[async_trait]
pub trait ResolveHandler<E: Event, T>: 'static + Sync + Send {
    const HANDLER_TYPE: &'static str;
    type Error: Debug + Sync + Send + 'static;
    async fn apply(entity: &mut Option<T>, event: E) -> Result<(), Self::Error>;
}

#[async_trait]
pub trait Resolver<T>: 'static + Sync + Send {
    async fn resolve(&self, entity: &mut Option<T>, payload: &[u8]) -> Result<(), ResolveError>;
}

pub(crate) struct TypedResolver<E: Event, T, H> {
    _event: PhantomData<E>,
    _resolve: PhantomData<T>,
    _handler: PhantomData<H>,
}

impl<E: Event, T, H: ResolveHandler<E, T>> Default for TypedResolver<E, T, H> {
    fn default() -> Self {
        Self {
            _event: Default::default(),
            _resolve: Default::default(),
            _handler: Default::default(),
        }
    }
}

#[async_trait]
impl<E: Event, T: ResolveMapping, H> Resolver<T> for TypedResolver<E, T, H>
where
    H: ResolveHandler<E, T>,
{
    async fn resolve(&self, entity: &mut Option<T>, payload: &[u8]) -> Result<(), ResolveError> {
        let event = E::from_bytes(payload)?;
        if let Err(reason) = H::apply(entity, event).await {
            tracing::error!("{:?}", reason);
            return Err(ResolveError::InProcess {
                trace: format!("{:?}", reason),
            });
        }
        Ok(())
    }
}
