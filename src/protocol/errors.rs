#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("")]
    Read,
    #[error("")]
    Write
}