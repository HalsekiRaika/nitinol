mod write;
mod read;

use std::error::Error;

#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("Failed setup database: {0}")]
    Setup(#[source] Box<dyn Error + Sync + Send>),
    #[error("Failed to write data: {0}")]
    Write(#[source] Box<dyn Error + Sync + Send>),
    #[error("Failed to read data: {0}")]
    Read(#[source] Box<dyn Error + Sync + Send>),
}
