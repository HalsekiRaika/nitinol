use serde::{Deserialize, Serialize};
use spectrum::errors::{DeserializeError, SerializeError};
use spectrum::{Event, Projection};
use spectrum::mapping::{Mapper, ResolveMapping};

pub struct Counter {
    state: u32,
}

impl ResolveMapping for Counter {
    fn mapping(mapper: &mut Mapper<Self>) {
        mapper.register::<CounterEvent>();
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

#[derive(Debug, Deserialize, Serialize)]
pub enum CounterEvent {
    Increased,
    Decreased
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
