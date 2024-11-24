use crate::any::{AnyRef, InvalidCast};
use crate::identifier::{EntityId, ToEntityId};
use crate::{lifecycle, Context, Process, Ref};
use async_trait::async_trait;
use std::collections::HashMap;
use std::error::Error;
use std::sync::Arc;
use tokio::sync::RwLock;

pub struct Registry {
    registry: Arc<RwLock<HashMap<EntityId, AnyRef>>>
}

#[async_trait]
pub trait ProcessSystem: 'static + Sync + Send {
    async fn spawn<T: Process>(&self, id: impl ToEntityId, entity: T, seq: i64) -> Result<Ref<T>, RegistryError>;
    async fn find<T: Process>(&self, id: impl ToEntityId) -> Result<Option<Ref<T>>, InvalidCast>;
}

#[async_trait]
impl ProcessSystem for Registry {
    async fn spawn<T: Process>(&self, id: impl ToEntityId, entity: T, seq: i64) -> Result<Ref<T>, RegistryError> {
        lifecycle::run(id, entity, Context::new(seq, self.clone()), self.clone()).await
    }
    
    async fn find<T: Process>(&self, id: impl ToEntityId) -> Result<Option<Ref<T>>, InvalidCast> {
        self.find::<T>(&id.to_entity_id()).await
    }
}


impl Registry {
    pub(crate) async fn register<T: Process>(
        &self,
        id: EntityId,
        writer: Ref<T>,
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

    #[rustfmt::skip]
    pub(crate) async fn find<T: Process>(&self, id: &EntityId) -> Result<Option<Ref<T>>, InvalidCast> {
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
    #[error(transparent)]
    TrySpawn(Box<dyn Error + Sync + Send>)
}

impl Clone for Registry {
    fn clone(&self) -> Self {
        Self { registry: Arc::clone(&self.registry) }
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self {
            registry: Arc::new(RwLock::new(HashMap::new()))
        }
    }
}