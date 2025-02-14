use nitinol_core::identifier::EntityId;

#[derive(Debug, thiserror::Error)]
pub enum ProjectionError {
    #[error("Failed to read protocol. {0}")]
    Protocol(Box<dyn std::error::Error + Send + Sync>),
}

#[derive(Debug, thiserror::Error)]
pub enum RejectProjection {
    #[error("Projection interrupted. This is a user defined behavior.")]
    Interrupted,
}

#[derive(Debug, thiserror::Error)]
#[error("There are data incompatible with Mapping. key:{key}")]
pub struct NotCompatible {
    pub key: String
}

#[derive(Debug, thiserror::Error)]
#[error("Failed projection. entity:{id}")]
pub struct FailedProjection {
    pub id: EntityId
}

#[derive(Debug, thiserror::Error)]
#[error("Failed projection. keys:{keys}")]
pub struct FailedProjectionWithKey {
    pub keys: String
}
