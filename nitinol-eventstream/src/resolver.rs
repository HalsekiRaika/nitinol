use std::collections::HashMap;
use std::marker::PhantomData;
use std::sync::Arc;

use async_trait::async_trait;
use nitinol_core::event::Event;
use nitinol_process::Ref;
use crate::extension::WithEventSubscriber;
use crate::subscriber::EventSubscriber;

pub trait SubscriptionMapper: 'static + Sync + Send + Sized {
    fn mapping(mapping: &mut DecodeMapping<Self>);
}

pub struct DecodeMapping<S> {
    pub(crate) map: HashMap<String, Arc<dyn Decoder<S>>>
}

impl<S> Default for DecodeMapping<S> {
    fn default() -> Self {
        Self { map: HashMap::default() }
    }
}

impl<S> DecodeMapping<S> {
    pub fn register<E: Event>(&mut self) -> &mut Self 
    where
        S: EventSubscriber<E>
    {
        self.map.insert(E::REGISTRY_KEY.to_string(), Arc::new(SubscribeResolver::default()));
        self
    }
}

#[async_trait]
pub(crate) trait Decoder<S>: 'static + Sync + Send {
    async fn apply(&self, subscriber: &mut S, payload: &[u8]);
}

pub(crate) struct DecodeResolver<E: Event, S: WithEventSubscriber<E>> {
    _event: PhantomData<E>,
    subscriber: Ref<S>,
}

impl<E: Event, S: WithEventSubscriber<E>> DecodeResolver<E, S> {
    pub fn new(subscriber: Ref<S>) -> Self {
        Self {
            _event: Default::default(),
            subscriber,
        }
    }
}

impl<E: Event, S> DecodeResolver<E, S> 
where 
    S: WithEventSubscriber<E>
{
    pub(crate) async fn apply(&self, payload: &[u8]) {
        let event = E::from_bytes(payload).unwrap();
        let Ok(command) = S::Command::try_from(event) else {
            return;
        };
        match self.subscriber.employ(command).await {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => {
                tracing::error!("{e:?}");
            }
            Err(e) => {
                tracing::error!("{e:?}");
            }
        }
    }
}

pub(crate) struct SubscribeResolver<E: Event, S> {
    _event: PhantomData<E>,
    _subscriber: PhantomData<S>,
}

impl<E: Event, S> Default for SubscribeResolver<E, S> {
    fn default() -> Self {
        Self {
            _event: Default::default(),
            _subscriber: Default::default(),
        }
    }
}

#[async_trait]
impl<E: Event, S> Decoder<S> for SubscribeResolver<E, S> 
where
    S: EventSubscriber<E>
{
    async fn apply(&self, subscriber: &mut S, payload: &[u8]) {
        let event = E::from_bytes(payload).unwrap();
        if let Err(e) = subscriber.on(event).await {
            tracing::error!("{e:?}");
        }
    }
}
