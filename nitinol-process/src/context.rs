mod status;

pub use status::*;

use crate::registry::ProcessRegistry;
use crate::{Process, Receptor};

use nitinol_core::identifier::EntityId;

pub struct Context {
    pub(crate) sequence: i64,
    pub(crate) status: Status,
    pub(crate) registry: ProcessRegistry,
}

impl Context {
    pub fn new(sequence: i64, registry: ProcessRegistry) -> Context {
        Self { sequence, status: Status::new(true), registry }
    }
}

impl Context {
    pub fn sequence(&self) -> i64 {
        self.sequence
    }
    
    pub fn status(&self) -> &Status {
        &self.status
    }

    pub async fn is_active(&self) -> bool {
        self.status.is_active().await
    }

    pub async fn poison(&self) {
        self.status.poison().await;
    }
    
    pub fn registry(&self) -> &ProcessRegistry {
        &self.registry
    }
    
    pub async fn find<T: Process>(&self, id: &EntityId) -> Option<Receptor<T>> {
        self.registry.find::<T>(id).await.unwrap()
    }
}
