use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use nitinol::process::manager::ProcessManager;
use nitinol::process::{Applicator, Context, Publisher, Process};
use nitinol::macros::{Command, Event};

#[derive(Debug, Clone, Command)]
pub enum DomainCommand {
    ChangeName { new: String }
}

#[derive(Debug, Clone, Event, Deserialize, Serialize)]
#[persist(enc = "serde_json::to_vec", dec = "serde_json::from_slice")]
pub enum DomainEvent {
    ChangedName { new: String }
}

#[derive(Debug, Clone)]
pub struct Aggregate {
    name: String
}

impl Process for Aggregate {}

#[async_trait]
impl Publisher<DomainCommand> for Aggregate {
    type Event = DomainEvent;
    type Rejection = anyhow::Error;
    
    #[allow(unreachable_patterns)]
    #[allow(clippy::match_single_binding)]
    async fn publish(&self, command: DomainCommand, _: &mut Context) -> Result<Self::Event, Self::Rejection> {
        match command { 
            DomainCommand::ChangeName { new } => Ok(DomainEvent::ChangedName { new })
        }
    }
}

#[async_trait]
impl Applicator<DomainEvent> for Aggregate {
    #[allow(unreachable_patterns)]
    #[allow(clippy::match_single_binding)]
    async fn apply(&mut self, event: DomainEvent, _: &mut Context) {
        match event {
            DomainEvent::ChangedName { new } => {
                self.name = new;
            }
        }
    }
}


#[tokio::test]
async fn main() -> Result<(), anyhow::Error> {
    let system = ProcessManager::default();
    
    let aggregate = Aggregate { name: "name".to_string() };
    let refs = system.spawn("aggregate", aggregate, 0).await?;
    
    let ev = refs.publish(DomainCommand::ChangeName { new: "new name".to_string() }).await??;
    refs.apply(ev).await?;
    
    Ok(())
}
