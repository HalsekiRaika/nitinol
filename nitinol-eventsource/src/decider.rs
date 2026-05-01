use async_trait::async_trait;

use crate::aggregate::Aggregate;
use crate::context::Context;
use crate::Effect;

#[async_trait]
pub trait Decider<C>: Aggregate
where
    C: Send + Sync + 'static,
{
    type Rejection: std::error::Error + Send + Sync + 'static;

    async fn decide(
        &self,
        cmd: C,
        ctx: &mut Context,
    ) -> Result<Effect<Self::Event>, Self::Rejection>;
}
