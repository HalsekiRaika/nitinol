use serde::{Deserialize, Serialize};
use spectroscopy::errors::{DeserializeError, SerializeError};
use spectroscopy::mapping::{Mapper, ResolveMapping};
use spectroscopy::{Event, Projection};

pub struct Counter {
    pub state: u32,
}

impl ResolveMapping for Counter {
    fn mapping(mapper: &mut Mapper<Self>) {
        mapper.register::<CounterEvent>();
    }
}

#[async_trait::async_trait]
impl Publisher<CounterCommand> for Counter {
    type Event = CounterEvent;
    type Rejection = ();

    async fn publish(&self, command: CounterCommand, _: &mut Context) -> Result<Self::Event, Self::Rejection> {
        match command {
            CounterCommand::Increase => Ok(CounterEvent::Increased),
            CounterCommand::Decrease => Ok(CounterEvent::Decreased)
        }
    }
}

#[async_trait::async_trait]
impl Applicator<CounterEvent> for Counter {
    async fn apply(&mut self, event: CounterEvent, _: &mut Context) {
        match event {
            CounterEvent::Increased => self.state += 1,
            CounterEvent::Decreased => self.state -= 1,
        }
    }
}

#[async_trait::async_trait]
impl Projection<CounterEvent> for Counter {
    type Rejection = ();

    async fn first(event: CounterEvent) -> Result<Self, Self::Rejection> {
        let mut state = 0;

        match event {
            CounterEvent::Increased => state += 1,
            CounterEvent::Decreased => state -= 1,
        }

        Ok(Self { state })
    }

    async fn apply(&mut self, event: CounterEvent) -> Result<(), Self::Rejection> {
        match event {
            CounterEvent::Increased => self.state += 1,
            CounterEvent::Decreased => self.state -= 1,
        }
        Ok(())
    }
}

#[derive(Debug)]
pub enum CounterCommand {
    Increase,
    Decrease
}

impl Command for CounterCommand {}

#[derive(Debug, Deserialize, Serialize)]
pub enum CounterEvent {
    Increased,
    Decreased,
}

impl Event for CounterEvent {
    const REGISTRY_KEY: &'static str = "counter-event";

    fn as_bytes(&self) -> Result<Vec<u8>, SerializeError> {
        Ok(serde_json::to_vec(self)?)
    }

    fn from_bytes(bytes: &[u8]) -> Result<Self, DeserializeError> {
        Ok(serde_json::from_slice(bytes)?)
    }
}



use spectroscopy::agent::{AgentExecutor, Applicator, Command, Context, Executor, Publisher};

#[tokio::test]
async fn spawn_agent() {
    let executor = Executor::default();

    let id = "counter-1".to_string();
    let counter = Counter {
        state: 0,
    };

    let agent = executor.spawn(id, counter).await.unwrap();
    
    let cmd1 = CounterCommand::Increase;
    let cmd2 = CounterCommand::Decrease;
    let ev1 = agent.publish(cmd1).await.unwrap().unwrap();
    let ev2 = agent.publish(cmd2).await.unwrap().unwrap();
    
    agent.apply(ev1).await.unwrap();
    agent.apply(ev2).await.unwrap();
}