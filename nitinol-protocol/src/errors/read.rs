use nitinol_core::identifier::EntityId;

#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    #[error("Not found event. entity:{0}")]
    NotFound(EntityId)
}