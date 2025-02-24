use std::fmt::Debug;
use async_trait::async_trait;
use nitinol_core::command::Command;
use nitinol_core::event::Event;
use nitinol_core::identifier::EntityId;
use nitinol_process::{Applicator, Context, FromContextExt, Process, Publisher};
use crate::extension::EventStreamExtension;

#[async_trait]
pub trait WithEventSubscriber<E: Event>: 'static + Sync + Send 
where
    Self: Process
        + Publisher<<Self as WithEventSubscriber<E>>::Command, Rejection: Debug>
        + Applicator<Self::Event>,
{
    type Command: TryFrom<E> + Command;
    
    #[tracing::instrument(skip_all, fields(id = %self.aggregate_id()))]
    async fn subscribe(&self, ctx: &mut Context) {
        let Ok(ext) = EventStreamExtension::from_context(ctx) else {
            panic!("`EventStreamExtension` not installed in context");
        };
        let refs = ctx.registry()
            .find::<Self>(&self.aggregate_id())
            .await
            .unwrap()
            .expect("What ???? I'm sure it's exist");
        
        ext.0.subscribe_to_process(refs).await;
    }
}
