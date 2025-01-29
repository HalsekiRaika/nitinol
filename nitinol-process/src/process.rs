use async_trait::async_trait;
use crate::Context;

#[allow(unused_variables)]
#[async_trait]
pub trait Process: 'static + Sync + Send + Sized {
    async fn start(&self, ctx: &mut Context) {}
    async fn stop(&self, ctx: &mut Context) {}
}
