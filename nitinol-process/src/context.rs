use crate::extension::errors::Missing;
use crate::extension::Extensions;
use crate::registry::ProcessRegistry;
use std::sync::Arc;
use tokio::sync::RwLock;


pub struct Context {
    pub(crate) sequence: i64,
    pub(crate) is_active: Arc<RwLock<bool>>,
    pub(crate) registry: ProcessRegistry,
    pub(crate) extension: Extensions,
}

impl Context {
    pub fn new(sequence: i64, registry: ProcessRegistry, extension: Extensions) -> Context {
        Self { sequence, is_active: Arc::new(RwLock::new(true)), registry, extension }
    }
}

impl Context {
    pub fn sequence(&self) -> i64 {
        self.sequence
    }

    pub async fn is_active(&self) -> bool {
        *self.is_active.read().await
    }

    pub async fn poison_pill(&self) {
        let mut guard = self.is_active.write().await;
        *guard = false;
    }
    
    pub fn registry(&self) -> &ProcessRegistry {
        &self.registry
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