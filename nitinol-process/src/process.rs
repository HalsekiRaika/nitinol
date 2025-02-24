use async_trait::async_trait;
use nitinol_core::identifier::EntityId;
use crate::{Context, Ref};

#[allow(unused_variables)]
#[async_trait]
pub trait Process: 'static + Sync + Send + Sized {
    fn aggregate_id(&self) -> EntityId;
    async fn start(&self, ctx: &mut Context) {}
    async fn stop(&self, ctx: &mut Context) {}
    
    async fn as_ref_self(&self, ctx: &Context) -> Option<Ref<Self>> {
        ctx.find(&self.aggregate_id()).await
    }
}
