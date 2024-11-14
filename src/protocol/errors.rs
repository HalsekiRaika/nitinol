use std::error::Error;

#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error(transparent)]
    Read(Box<dyn Error + Sync + Send>),
    #[error(transparent)]
    Write(Box<dyn Error + Sync + Send>),
}
