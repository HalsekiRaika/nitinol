use async_trait::async_trait;
use crate::agent::{lifecycle, Agent, Context, Registry, RegistryError};
use crate::identifier::ToEntityId;
use crate::mapping::ResolveMapping;

#[derive(Debug, Clone, Default)]
pub struct Executor {
    registry: Registry,
}

#[async_trait]
impl AgentExecutor for Executor {
    async fn spawn<T: ResolveMapping>(&self, id: impl ToEntityId, entity: T) -> Result<Agent<T>, ExecutorError> {
        let ctx = Context { sequence: 0 };
        let agent = lifecycle::spawn(id.to_entity_id(), entity, ctx, &self.registry).await
            .map_err(ExecutorError::Registry)?;
        Ok(agent)
    }
}


#[derive(Debug, thiserror::Error)]
pub enum ExecutorError {
    #[error(transparent)]
    Registry(RegistryError)
}

#[async_trait]
pub trait AgentExecutor: 'static + Sync + Send {
    async fn spawn<T: ResolveMapping>(&self, id: impl ToEntityId, entity: T) -> Result<Agent<T>, ExecutorError>;
}

