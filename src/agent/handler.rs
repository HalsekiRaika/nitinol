use std::sync::Arc;
use async_trait::async_trait;
use crate::agent::{lifecycle, Agent, Context, Registry, RegistryError};
use crate::agent::extension::Extensions;
use crate::identifier::ToEntityId;
use crate::mapping::ResolveMapping;

#[derive(Clone, Default)]
pub struct Executor {
    registry: Registry,
    extensions: Arc<Extensions>
}

impl Executor {
    pub fn build_ext(f: impl FnOnce(&mut Extensions)) -> Executor {
        let mut ext = Extensions::default();
        f(&mut ext);
        Executor { 
            registry: Registry::default(), 
            extensions: Arc::new(ext) 
        }
    }
}


#[async_trait]
impl AgentExecutor for Executor {
    async fn spawn<T: ResolveMapping>(&self, id: impl ToEntityId, entity: T, seq: i64) -> Result<Agent<T>, ExecutorError> {
        let ctx = Context { sequence: seq, extension: Arc::clone(&self.extensions) };
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
    async fn spawn<T: ResolveMapping>(&self, id: impl ToEntityId, entity: T, seq: i64) -> Result<Agent<T>, ExecutorError>;
}

