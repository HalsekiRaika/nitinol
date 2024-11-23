#[derive(Debug, thiserror::Error)]
pub enum ProcessError {
    #[error("Channel dropped")]
    ChannelDropped,
}