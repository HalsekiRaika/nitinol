#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("Incompatible downcast.")]
    Downcast,
    #[error("channel was dropped.")]
    ChannelDropped,
}
