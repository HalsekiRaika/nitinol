use nitinol_core::identifier::ToEntityId;
use crate::extension::{Extensions, Installer};
use crate::extension::errors::AlreadyInstalled;
use crate::{lifecycle, Context, Process, Ref};
use crate::errors::{AlreadyExist, InvalidCast};
use crate::registry::ProcessRegistry;

#[derive(Clone)]
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

    pub async fn spawn<T: Process>(&self, id: impl ToEntityId, entity: T, seq: i64) -> Result<Ref<T>, AlreadyExist> {
        lifecycle::run(id, entity, Context::new(seq, self.registry.clone(), self.extension.clone()), self.registry.clone()).await
    }

    pub async fn find<T: Process>(&self, id: impl ToEntityId) -> Result<Option<Ref<T>>, InvalidCast> {
        self.registry.track::<T>(&id.to_entity_id()).await
    }
}

impl Default for ProcessManager {
    fn default() -> Self {
        Self {
            extension: Extensions::default(),
            registry: ProcessRegistry::default()
        }
    }
}