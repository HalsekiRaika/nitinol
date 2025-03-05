mod status;

pub use status::*;

use crate::extension::errors::Missing;
use crate::extension::Extensions;
use crate::registry::ProcessRegistry;
use crate::{Process, Ref};

use nitinol_core::identifier::EntityId;

pub struct Context {
    pub(crate) sequence: i64,
    pub(crate) status: Status,
    pub(crate) registry: ProcessRegistry,
    pub(crate) extension: Extensions,
}

impl Context {
    pub fn new(sequence: i64, registry: ProcessRegistry, extension: Extensions) -> Context {
        Self { sequence, status: Status::new(true), registry, extension }
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
    
    pub async fn find<T: Process>(&self, id: &EntityId) -> Option<Ref<T>> {
        self.registry.find::<T>(id).await.unwrap()
    }

    pub fn extension(&self) -> &Extensions {
        &self.extension
    }
}

pub trait FromContextExt: 'static + Sync + Send + Clone {
    fn from_context(ctx: &Context) -> Result<&Self, Missing> {
        ctx.extension().get::<Self>()
    }
}