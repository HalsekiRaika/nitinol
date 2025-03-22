use std::time::Duration;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use uuid::Uuid;
use nitinol::{Command, Event};
use nitinol_core::identifier::{EntityId, ToEntityId};
use nitinol_eventstream::eventstream::EventStream;
use nitinol_eventstream::process::resolver::SubscribeProcess;
use nitinol_eventstream::process::{WithEventSubscriber, WithStreamPublisher};
use nitinol_process::{EventApplicator, Context, Process, CommandHandler, Receptor};
use nitinol_process::manager::ProcessManager;
use nitinol_resolver::mapping::Mapper;
use nitinol_resolver::mapping::process::WithResolveMapping;

#[derive(Debug, Clone, Command)]
pub struct TestCommand(Uuid);

#[derive(Debug, Clone, Command)]
pub struct AnotherTestCommand(Uuid);

#[derive(Debug, Clone, Event, Deserialize, Serialize)]
#[persist(enc = "serde_json::to_vec", dec = "serde_json::from_slice")]
pub struct TestEvent(Uuid);

impl TryFrom<TestEvent> for TestCommand {
    type Error = anyhow::Error;

    fn try_from(val: TestEvent) -> Result<Self, Self::Error> {
        Ok(Self(val.0))
    }
}

#[derive(Debug, Clone, Event, Deserialize, Serialize)]
#[persist(enc = "serde_json::to_vec", dec = "serde_json::from_slice")]
pub struct AnotherTestEvent(Uuid);

impl TryFrom<AnotherTestEvent> for TestCommand {
    type Error = anyhow::Error;
    
    fn try_from(val: AnotherTestEvent) -> Result<Self, Self::Error> {
        Ok(Self(val.0))
    }
}

pub struct TestProcess {
    id: Uuid
}

#[async_trait]
impl Process for TestProcess {
    fn aggregate_id(&self) -> EntityId {
        self.id.to_entity_id()
    }
    
    async fn start(&self, ctx: &mut Context) {
        self.subscribe(ctx).await;
    }
}

impl WithResolveMapping for TestProcess {
    fn mapping(mapper: &mut Mapper<Self>, myself: Receptor<Self>) {
        mapper
            .register_with::<AnotherTestEvent, _>(SubscribeProcess::new(myself.clone()));
    }
}

impl WithEventSubscriber<TestEvent> for TestProcess {
    type Command = TestCommand;
}

#[async_trait]
impl CommandHandler<TestCommand> for TestProcess {
    type Event = TestEvent;
    type Rejection = anyhow::Error;
    
    #[tracing::instrument(skip_all, name = "TestProcess::publish", fields(id = %self.aggregate_id()))]
    async fn handle(&self, cmd: TestCommand, _: &mut Context) -> Result<Self::Event, Self::Rejection> {
        tracing::debug!("Publish event, {:?}", cmd);
        Ok(TestEvent(cmd.0))
    }
}


#[async_trait]
impl EventApplicator<TestEvent> for TestProcess {
    #[tracing::instrument(skip_all, name = "TestProcess::apply", fields(id = %self.aggregate_id()))]
    async fn apply(&mut self, event: TestEvent, _: &mut Context) {
        tracing::debug!("Apply event: {:?}", event);
    }
}


pub struct AnotherTestProcess {
    id: Uuid
}

impl Process for AnotherTestProcess {
    fn aggregate_id(&self) -> EntityId {
        self.id.to_entity_id()
    }
}


#[async_trait]
impl CommandHandler<AnotherTestCommand> for AnotherTestProcess {
    type Event = AnotherTestEvent;
    type Rejection = anyhow::Error;
    
    #[tracing::instrument(skip_all, name = "AnotherTestProcess::publish", fields(id = %self.aggregate_id()))]
    async fn handle(&self, command: AnotherTestCommand, _: &mut Context) -> Result<Self::Event, Self::Rejection> {
        tracing::debug!("Publish event, {:?}", command);
        Ok(AnotherTestEvent(command.0))
    }
}

#[async_trait]
impl EventApplicator<AnotherTestEvent> for AnotherTestProcess {
    #[tracing::instrument(skip_all, name = "AnotherTestProcess::apply", fields(id = %self.aggregate_id()))]
    async fn apply(&mut self, event: AnotherTestEvent, ctx: &mut Context) {
        tracing::debug!("Apply event: {:?}", event);
        self.publish(&event, ctx).await;
    }
}

#[tokio::test]
async fn subscribe_event_from_root() -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(EnvFilter::new("trace"))
        .with(tracing_subscriber::fmt::layer())
        .init();
    
    
    let eventstream = EventStream::default();
    
    nitinol_eventstream::init_eventstream(eventstream.clone());
    
    let system = ProcessManager::default();
    
    // Subscribers
    system.spawn(TestProcess { id: Uuid::new_v4() }, 0).await?;
    system.spawn(TestProcess { id: Uuid::new_v4() }, 0).await?;
    system.spawn(TestProcess { id: Uuid::new_v4() }, 0).await?;
    
    tokio::time::sleep(Duration::from_secs(3)).await;
    
    let event1 = AnotherTestEvent(Uuid::new_v4());
    let event2 = AnotherTestEvent(Uuid::new_v4());
    let event3 = AnotherTestEvent(Uuid::new_v4());
    
    tokio::join!(
        eventstream.publish("root".to_entity_id(), 0, &event1),
        eventstream.publish("root".to_entity_id(), 1, &event2),
        eventstream.publish("root".to_entity_id(), 2, &event3),
    );
    
    tokio::time::sleep(Duration::from_secs(3)).await;
    
    Ok(())
}

#[tokio::test]
async fn subscribe_event_from_process() -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(EnvFilter::new("trace"))
        .with(tracing_subscriber::fmt::layer())
        .init();
    
    let eventstream = EventStream::default();
    
    nitinol_eventstream::init_eventstream(eventstream.clone());
    
    let system = ProcessManager::default();
    
    // Subscribers
    system.spawn(TestProcess { id: Uuid::new_v4() }, 0).await?;
    system.spawn(TestProcess { id: Uuid::new_v4() }, 0).await?;
    system.spawn(TestProcess { id: Uuid::new_v4() }, 0).await?;
    
    // Publisher
    let refs = system.spawn(AnotherTestProcess { id: Uuid::new_v4() }, 0).await?;
    
    refs.entrust(AnotherTestCommand(Uuid::new_v4())).await?;
    refs.entrust(AnotherTestCommand(Uuid::new_v4())).await?;
    refs.entrust(AnotherTestCommand(Uuid::new_v4())).await?;
    
    tokio::time::sleep(Duration::from_secs(3)).await;
    
    Ok(())
}

#[tokio::test]
#[should_panic]
async fn not_installed_eventstream() {
    tracing_subscriber::registry()
        .with(EnvFilter::new("trace"))
        .with(tracing_subscriber::fmt::layer())
        .init();
    
    let system = ProcessManager::default();
    
    system.spawn(TestProcess { id: Uuid::new_v4() }, 0).await.unwrap();
    
    let refs = system.spawn(AnotherTestProcess { id: Uuid::new_v4() }, 0).await.unwrap();
    
    let ev = refs.handle(AnotherTestCommand(Uuid::new_v4())).await.unwrap().unwrap();
    
    // panic here
    refs.apply(ev).await.unwrap();
}
