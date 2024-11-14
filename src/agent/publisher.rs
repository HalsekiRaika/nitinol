use crate::agent::{Applier, Command, Context};
use crate::mapping::ResolveMapping;
use crate::Event;
use async_trait::async_trait;
use tokio::sync::oneshot;

#[async_trait]
pub trait Publisher<C: Command>: 'static + Sync + Send {
    type Event: Event;
    type Rejection: 'static + Sync + Send;
    async fn publish(&self, command: C, ctx: &mut Context) -> Result<Self::Event, Self::Rejection>;
}

pub(crate) struct PublishHandler<C: Command, T: ResolveMapping>
where
    T: Publisher<C>,
{
    pub(crate) command: C,
    pub(crate) oneshot: oneshot::Sender<Result<T::Event, T::Rejection>>,
}

#[async_trait::async_trait]
impl<C: Command, T: ResolveMapping> Applier<T> for PublishHandler<C, T>
where
    T: Publisher<C>,
{
    async fn apply(
        self: Box<Self>,
        entity: &mut T,
        ctx: &mut Context,
    ) -> Result<(), crate::agent::errors::AgentError> {
        self.oneshot
            .send(entity.publish(self.command, ctx).await)
            .map_err(|_| crate::agent::errors::AgentError::ChannelDropped)
    }
}

#[cfg(test)]
mod test {
    #![allow(unused)]

    use super::Publisher;
    use crate::agent::{Command, Context};
    use crate::errors::{DeserializeError, SerializeError};
    use crate::Event;
    use async_trait::async_trait;
    use serde::{Deserialize, Serialize};

    pub struct User {
        id: String,

        // Length Limit 8
        name: String,
    }

    pub enum UserCommand {
        ChangeName { to: String },
    }

    impl Command for UserCommand {}

    #[derive(Debug, Eq, PartialEq, Deserialize, Serialize)]
    pub enum UserEvent {
        ChangedName { changed: String },
    }

    impl Event for UserEvent {
        const REGISTRY_KEY: &'static str = "user-event";

        fn as_bytes(&self) -> Result<Vec<u8>, SerializeError> {
            unimplemented!()
        }

        fn from_bytes(bytes: &[u8]) -> Result<Self, DeserializeError> {
            unimplemented!()
        }
    }

    #[derive(Debug)]
    pub enum PublishError {
        Violation,
    }

    #[async_trait]
    impl Publisher<UserCommand> for User {
        type Event = UserEvent;
        type Rejection = PublishError;

        async fn publish(
            &self,
            command: UserCommand,
            _: &mut Context,
        ) -> Result<Self::Event, Self::Rejection> {
            match command {
                UserCommand::ChangeName { to } => {
                    if to.len() >= 8 && self.name.eq(&to) {
                        return Err(PublishError::Violation);
                    }

                    Ok(UserEvent::ChangedName { changed: to })
                }
            }
        }
    }

    #[tokio::test]
    async fn publish() {
        let mut ctx = Context { sequence: 0 };
        let user = User {
            id: "aaa".to_string(),
            name: "test man".to_string(),
        };
        let cmd = UserCommand::ChangeName {
            to: "testing man".to_string(),
        };
        let event = user.publish(cmd, &mut ctx).await.unwrap();

        assert_eq!(
            event,
            UserEvent::ChangedName {
                changed: "testing man".to_string()
            }
        );
    }
}
