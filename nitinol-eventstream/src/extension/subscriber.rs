use crate::extension::EventStreamExtension;
use async_trait::async_trait;
use nitinol_core::command::Command;
use nitinol_core::event::Event;
use nitinol_process::{Applicator, Context, FromContextExt, Publisher};
use std::fmt::Debug;
use nitinol_resolver::mapping::Mapper;
use nitinol_resolver::mapping::process::WithResolveMapping;

#[async_trait]
pub trait WithEventSubscriber<E: Event>: 'static + Sync + Send
where
    Self: WithResolveMapping
        + Publisher<<Self as WithEventSubscriber<E>>::Command, Rejection: Debug>
        + Applicator<Self::Event>,
{
    type Command: TryFrom<E, Error: Debug> + Command;
    
    #[tracing::instrument(skip_all, fields(id = %self.aggregate_id()))]
    async fn subscribe(&self, ctx: &mut Context) {
        let Ok(ext) = EventStreamExtension::from_context(ctx) else {
            panic!("`EventStreamExtension` not installed in context");
        };
        
        let Some(refs) = self.as_ref_self(ctx).await else {
            panic!("`Process=[{}]` not found in registry", self.aggregate_id());
        };
        
        let mut mapping: Mapper<Self> = Mapper::default();
        Self::mapping(&mut mapping, refs);
        
        ext.0.process_subscribe(mapping, ctx.status().clone()).await;
        
        tracing::debug!("subscribe start.")
    }
}


pub mod resolver {
    use std::marker::PhantomData;
    
    use async_trait::async_trait;
    use nitinol_core::event::Event;
    use nitinol_process::Ref;
    use nitinol_resolver::errors::ResolveError;
    use nitinol_resolver::resolver::{Resolver, ResolverType};
    
    use super::WithEventSubscriber;
    
    pub const RESOLVE_TYPE: &str = "process-subscriber";
    
    pub struct SubscribeProcess<E: Event, S: WithEventSubscriber<E>> {
        _event: PhantomData<E>,
        subscriber: Ref<S>,
    }
    
    impl<E: Event, S: WithEventSubscriber<E>> SubscribeProcess<E, S> {
        pub fn new(subscriber: Ref<S>) -> Self {
            Self {
                _event: Default::default(),
                subscriber,
            }
        }
    }
    
    impl<E: Event, S> ResolverType<S> for SubscribeProcess<E, S> 
    where
        S: WithEventSubscriber<E>
    {
        const RESOLVE_TYPE: &'static str = RESOLVE_TYPE;
    }
    
    #[async_trait]
    impl<E: Event, S> Resolver<S> for SubscribeProcess<E, S>
    where
        S: WithEventSubscriber<E>
    {
        async fn resolve(&self, _: &mut Option<S>, payload: &[u8]) -> Result<(), ResolveError> {
            let ev = E::from_bytes(payload)?;
            let command = match S::Command::try_from(ev) {
                Ok(command) => command,
                Err(e) => return Err(ResolveError::InProcess {
                    trace: format!("{:?}", e),
                }),
            };
            
            match self.subscriber.employ(command).await {
                Ok(Ok(_)) => {}
                Err(e) => {
                    tracing::error!("{:?}", e);
                }
                Ok(Err(e)) => {
                    tracing::error!("{:?}", e);
                }
            }
            Ok(())
        }
    }
}
