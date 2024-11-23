#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("Channel dropped")]
    ChannelDropped,
}