use std::error::Error;

#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("Failed to write data")]
    Write(#[source] Box<dyn Error + Sync + Send>),
    #[error("Failed to read data")]
    Read(#[source] Box<dyn Error + Sync + Send>),
}