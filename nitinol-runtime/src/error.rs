use crate::ident::Pid;

#[derive(Debug, thiserror::Error)]
#[error("process already stopped")]
pub struct SendError;

#[derive(Debug, thiserror::Error)]
pub enum AskError<E: std::error::Error> {
    #[error("dead letter: no process at pid {destination}")]
    DeadLetter { destination: Pid },
    #[error("process dropped reply")]
    ReplyDropped,
    #[error("ask timed out: no reply from pid {destination}")]
    Timeout { destination: Pid },
    #[error(transparent)]
    Handler(E),
}

#[derive(Debug, thiserror::Error)]
#[error("stream topic '{topic}' already registered")]
pub struct SpawnError {
    pub topic: String,
}

#[derive(Debug, thiserror::Error)]
#[error("handler returned an error")]
pub struct HandlerError;
