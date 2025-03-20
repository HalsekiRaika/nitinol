use nitinol_core::identifier::ToEntityId;
use crate::extension::{Extensions, Installer};
use crate::extension::errors::AlreadyInstalled;
use crate::{lifecycle, Context, Process, Receptor};
use crate::errors::{AlreadyExist, InvalidCast};
use crate::registry::ProcessRegistry;

#[derive(Clone, Default)]
pub struct ProcessManager {
    extension: Extensions,
    registry: ProcessRegistry
}

impl ProcessManager {
    pub fn with_extension<F>(install: F) -> Result<Self, AlreadyInstalled> 
        where F: FnOnce(&mut Installer) -> Result<&mut Installer, AlreadyInstalled>
    {
        let mut installer = Extensions::builder();
        install(&mut installer)?;
        Ok(Self {
            extension: installer.build(),
            registry: ProcessRegistry::default()
        })
    }

    pub async fn spawn<T: Process>(&self, entity: T, seq: i64) -> Result<Receptor<T>, AlreadyExist> {
        lifecycle::run(entity.aggregate_id(), entity, Context::new(seq, self.registry.clone(), self.extension.clone()), self.registry.clone()).await
    }

    pub async fn find<T: Process>(&self, id: impl ToEntityId) -> Result<Option<Receptor<T>>, InvalidCast> {
        self.registry.find::<T>(&id.to_entity_id()).await
    }
}
