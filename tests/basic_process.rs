use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use nitinol::resolver::{ResolveMapping, Mapper};
use nitinol::process::{Applicator, Context, Publisher, Process};
use nitinol::macros::{Command, Event};

#[derive(Debug, Clone, Command)]
pub enum DomainCommand {
    
}

#[derive(Debug, Clone, Event, Deserialize, Serialize)]
#[persist(
    enc = "serde_json::to_vec",
    dec = "serde_json::from_slice"
)]
pub enum DomainEvent {
    
}

#[derive(Debug, Clone)]
pub struct Aggregate {
    
}

impl ResolveMapping for Aggregate {
    fn mapping(mapper: &mut Mapper<Self>) {
        todo!()
    }
}

impl Process for Aggregate {}

#[async_trait]
impl Publisher<DomainCommand> for Aggregate {
    type Event = DomainEvent;
    type Rejection = ();
    
    async fn publish(&self, command: DomainCommand, _: &mut Context) -> Result<Self::Event, Self::Rejection> {
        match command { 
            _ => todo!()
        }
    }
}

#[async_trait]
impl Applicator<DomainEvent> for Aggregate {
    async fn apply(&mut self, event: DomainEvent, _: &mut Context) {
        match event {
            _ => todo!()
        }
    }
}


#[tokio::test]
async fn main() {
    
}
