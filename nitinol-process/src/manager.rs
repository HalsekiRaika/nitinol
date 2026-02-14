use nitinol_core::identifier::ToEntityId;

use crate::errors::{AlreadyExist, InvalidCast};
use crate::registry::ProcessRegistry;
use crate::{lifecycle, Process, Receptor};

#[derive(Clone, Default)]
pub struct ProcessManager {
    registry: ProcessRegistry
}

impl ProcessManager {
    pub async fn spawn<T: Process>(&self, entity: T, start_seq: i64) -> Result<Receptor<T>, AlreadyExist> {
        lifecycle::run(entity.aggregate_id(), entity, start_seq, self.registry.clone(), None).await
    }

    pub async fn find<T: Process>(&self, id: impl ToEntityId) -> Result<Option<Receptor<T>>, InvalidCast> {
        self.registry.find::<T>(&id.to_entity_id()).await
    }
}
