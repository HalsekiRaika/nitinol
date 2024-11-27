use nitinol_core::identifier::EntityId;

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