use async_trait::async_trait;
use nitinol::{Command, EntityId, Event, ToEntityId};
use nitinol::process::manager::ProcessManager;
use nitinol::process::persistence::WithPersistence;
use nitinol::process::persistence::writer::EventWriter;
use nitinol::process::{CommandHandler, Context, EventApplicator, Process};
use nitinol_sqlite_adaptor::store::SqliteEventStore;
use serde::{Deserialize, Serialize};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

#[derive(Debug, Clone, Command)]
pub enum DomainCommand {
    ChangeName { new: String },
    Delete,
}

#[derive(Debug, Clone, Event, Deserialize, Serialize)]
#[persist(enc = "serde_json::to_vec", dec = "serde_json::from_slice")]
pub enum DomainEvent {
    ChangedName { new: String },
    Deleted,
}

#[derive(Debug, Clone)]
pub struct Aggregate {
    name: String
}

//noinspection DuplicatedCode
#[async_trait]
impl Process for Aggregate {
    fn aggregate_id(&self) -> EntityId {
        "aggregate".to_entity_id()
    }
    
    async fn start(&self, _: &mut Context) {
        tracing::debug!("Start: {:?}", self);
    }
    
    async fn stop(&self, _: &mut Context) {
        tracing::debug!("Stop: {:?}", self);
    }
}


//noinspection DuplicatedCode
#[async_trait]
impl CommandHandler<DomainCommand> for Aggregate {
    type Event = DomainEvent;
    type Rejection = anyhow::Error;
    
    #[tracing::instrument(skip_all)]
    async fn handle(&self, command: DomainCommand, _: &mut Context) -> Result<Self::Event, Self::Rejection> {
        let ev = match command {
            DomainCommand::ChangeName { new } => DomainEvent::ChangedName { new },
            DomainCommand::Delete => DomainEvent::Deleted,
        };
        tracing::debug!("Accept command. published event: {:?}", ev);
        Ok(ev)
    }
}

//noinspection DuplicatedCode
#[async_trait]
impl EventApplicator<DomainEvent> for Aggregate {
    #[tracing::instrument(skip_all)]
    async fn apply(&mut self, event: DomainEvent, ctx: &mut Context) {
        self.persist(&event, ctx).await;
        
        tracing::debug!("Accept event: {:?}", event);
        match event {
            DomainEvent::ChangedName { new } => {
                self.name = new;
            }
            DomainEvent::Deleted => {
                ctx.poison().await;
            }
        }
        tracing::debug!("current state: {:?}", self);
    }
}


#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    tracing_subscriber::registry()
        .with(EnvFilter::new("trace"))
        .with(tracing_subscriber::fmt::layer())
        .init();
    
    let eventstore = SqliteEventStore::setup("sqlite://:memory:").await?;
    let writer = EventWriter::new(eventstore).set_retry(5);
    
    nitinol::setup::set_writer(writer);
    
    let system = ProcessManager::default();
    
    let aggregate = Aggregate { name: "name".to_string() };
    let refs = system.spawn(aggregate, 0).await?;
    
    let ev = refs.handle(DomainCommand::ChangeName { new: "new name".to_string() }).await??;
    refs.apply(ev).await?;
    
    let ev = refs.handle(DomainCommand::Delete).await??;
    refs.apply(ev).await?;
    Ok(())
}
