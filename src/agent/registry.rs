use crate::agent::any::AnyAgent;
use crate::agent::errors::AgentError;
use crate::agent::Agent;
use crate::identifier::EntityId;
use crate::mapping::ResolveMapping;
use std::collections::HashMap;
use std::fmt::{Debug, Formatter};
use std::sync::Arc;
use tokio::sync::RwLock;

pub struct Registry {
    registry: Arc<RwLock<HashMap<EntityId, AnyAgent>>>,
}

impl Debug for Registry {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Registry").field("registry", &"..").finish()
    }
}

impl Clone for Registry {
    fn clone(&self) -> Self {
        Self {
            registry: Arc::clone(&self.registry),
        }
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self {
            registry: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

impl Registry {
    pub(crate) async fn register<T: ResolveMapping>(
        &self,
        id: EntityId,
        writer: Agent<T>,
    ) -> Result<(), RegistryError> {
        let mut lock = self.registry.write().await;
        if lock.iter().any(|(exist, _)| exist.eq(&id)) {
            return Err(RegistryError::AlreadyExist(id));
        }

        lock.insert(id, writer.into());

        Ok(())
    }

    pub(crate) async fn deregister(&self, id: &EntityId) -> Result<(), RegistryError> {
        let mut lock = self.registry.write().await;
        if !lock.iter().any(|(exist, _)| exist.eq(id)) {
            return Err(RegistryError::NotFound(id.to_owned()));
        }

        lock.remove(id);

        Ok(())
    }

    pub async fn find<T: ResolveMapping>(
        &self,
        id: &EntityId,
    ) -> Result<Option<Agent<T>>, AgentError> {
        let lock = self.registry.read().await;
        lock.iter()
            .find(|(dest, _)| dest.eq(&id))
            .map(|(_, any)| any.clone())
            .map(|owned| owned.downcast::<T>())
            .transpose()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("Already registered {0}")]
    AlreadyExist(EntityId),
    #[error("Not found Agent {0}")]
    NotFound(EntityId),
}
