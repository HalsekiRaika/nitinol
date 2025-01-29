use async_trait::async_trait;

#[async_trait]
pub trait Process: 'static + Sync + Send + Sized {
}

