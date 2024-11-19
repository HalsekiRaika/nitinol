#[derive(Debug, thiserror::Error)]
#[error("There are data incompatible with Mapping. key:{key}")]
pub struct NotCompatible {
    pub key: String
}