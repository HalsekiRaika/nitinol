use std::sync::Arc;
use tokio::sync::RwLock;

pub struct Status(Arc<RwLock<bool>>);

impl Clone for Status {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl Status {
    pub fn new(is_active: bool) -> Self {
        Self(Arc::new(RwLock::new(is_active)))
    }

    pub async fn is_active(&self) -> bool {
        *self.0.read().await
    }

    pub async fn poison(&self) {
        let mut guard = self.0.write().await;
        *guard = false;
    }
}
