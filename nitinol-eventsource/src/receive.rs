use async_trait::async_trait;

use crate::aggregate::Aggregate;
use crate::context::Context;

#[async_trait]
pub trait Receive<M>: Aggregate
where
    M: Send + Sync + 'static,
{
    type Response: Send + Sync + 'static;
    type Error: std::error::Error + Send + Sync + 'static;

    async fn recv(
        &self,
        msg: M,
        ctx: &mut Context,
    ) -> Result<Self::Response, Self::Error>;
}
