use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::RwLock;
use nitinol_core::identifier::EntityId;

use crate::any::AnyRef;
use crate::errors::{AlreadyExist, NotFound, InvalidCast};
use crate::{Process, Ref};

pub struct ProcessRegistry {
    registry: Arc<RwLock<HashMap<EntityId, AnyRef>>>
}

impl ProcessRegistry {
    pub(crate) async fn register<T: Process>(
        &self,
        id: EntityId,
        writer: Ref<T>,
    ) -> Result<(), AlreadyExist> {
        let lock = self.registry.read().await;
        if lock.iter().any(|(exist, _)| exist.eq(&id)) {
            return Err(AlreadyExist(id));
        }

        drop(lock); // release lock
        
        let mut lock = self.registry.write().await;
        lock.insert(id.clone(), writer.into());

        tracing::info!("Registered: {}", id);
        
        Ok(())
    }

    pub(crate) async fn deregister(&self, id: &EntityId) -> Result<(), NotFound> {
        let lock = self.registry.read().await;
        if !lock.iter().any(|(exist, _)| exist.eq(id)) {
            return Err(NotFound(id.to_owned()));
        }

        drop(lock); // release lock

        let mut lock = self.registry.write().await;
        lock.remove(id);

        tracing::info!("Deregistered: {}", id);
        
        Ok(())
    }

    #[rustfmt::skip]
    pub async fn find<T: Process>(&self, id: &EntityId) -> Result<Option<Ref<T>>, InvalidCast> {
        let lock = self.registry.read().await;
        lock.iter()
            .find(|(dest, _)| dest.eq(&id))
            .map(|(_, any)| any.clone())
            .map(|owned| owned.downcast::<T>())
            .transpose()
    }
}

impl Clone for ProcessRegistry {
    fn clone(&self) -> Self {
        Self { registry: Arc::clone(&self.registry) }
    }
}

impl Default for ProcessRegistry {
    fn default() -> Self {
        Self {
            registry: Arc::new(RwLock::new(HashMap::new()))
        }
    }
}