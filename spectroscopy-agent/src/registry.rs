use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use spectroscopy_core::resolver::ResolveMapping;
use crate::any::{AnyRef, InvalidCast};
use crate::identifier::EntityId;
use crate::Ref;

pub struct Registry {
    registry: Arc<RwLock<HashMap<EntityId, AnyRef>>>
}


impl Registry {
    pub(crate) async fn register<T: ResolveMapping>(
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

    pub async fn deregister(&self, id: &EntityId) -> Result<(), RegistryError> {
        let mut lock = self.registry.write().await;
        if !lock.iter().any(|(exist, _)| exist.eq(id)) {
            return Err(RegistryError::NotFound(id.to_owned()));
        }

        lock.remove(id);

        Ok(())
    }

    #[rustfmt::skip]
    pub async fn find<T: ResolveMapping>(&self, id: &EntityId) -> Result<Option<Ref<T>>, InvalidCast> {
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