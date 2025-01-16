use nitinol_core::identifier::EntityId;

#[derive(Debug, thiserror::Error)]
#[error("channel may have been dropped or thread may have been stopped.")]
pub struct ChannelDropped;

#[derive(Debug, thiserror::Error)]
#[error("Invalid cast to {to}")]
pub struct InvalidCast {
    pub to: &'static str
}

#[derive(Debug, thiserror::Error)]
#[error("Already registered {0} in registry")]
pub struct AlreadyExist(pub EntityId);

#[derive(Debug, thiserror::Error)]
#[error("Not found {0} in registry")]
pub struct NotFound(pub EntityId);
