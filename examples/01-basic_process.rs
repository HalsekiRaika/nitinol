use async_trait::async_trait;
use nitinol::{EntityId, ToEntityId};
use nitinol::{Command, Event};
use nitinol::process::manager::ProcessManager;
use nitinol::process::{Applicator, Context, Process, Publisher};
use serde::{Deserialize, Serialize};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

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

#[async_trait]
impl Publisher<DomainCommand> for Aggregate {
    type Event = DomainEvent;
    type Rejection = anyhow::Error;
    
    #[tracing::instrument(skip_all)]
    async fn publish(&self, command: DomainCommand, _: &mut Context) -> Result<Self::Event, Self::Rejection> {
        let ev = match command {
            DomainCommand::ChangeName { new } => DomainEvent::ChangedName { new },
            DomainCommand::Delete => DomainEvent::Deleted,
        };
        tracing::debug!("Accept command. published event: {:?}", ev);
        Ok(ev)
    }
}

#[async_trait]
impl Applicator<DomainEvent> for Aggregate {
    #[tracing::instrument(skip_all)]
    async fn apply(&mut self, event: DomainEvent, ctx: &mut Context) {
        tracing::debug!("Accept event: {:?}", event);
        match event {
            DomainEvent::ChangedName { new } => {
                self.name = new;
            }
            DomainEvent::Deleted => {
                ctx.poison_pill().await;
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
    
    let system = ProcessManager::default();
    
    let aggregate = Aggregate { name: "name".to_string() };
    let refs = system.spawn(aggregate, 0).await?;
    
    let ev = refs.publish(DomainCommand::ChangeName { new: "new name".to_string() }).await??;
    refs.apply(ev).await?;
    
    let ev = refs.publish(DomainCommand::Delete).await??;
    refs.apply(ev).await?;
    Ok(())
}
