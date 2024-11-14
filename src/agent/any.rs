use crate::agent::errors::AgentError;
use crate::agent::Agent;
use crate::mapping::ResolveMapping;
use std::any::Any;
use std::sync::Arc;

pub trait DynAgent: 'static + Sync + Send {
    fn as_any(&self) -> &dyn Any;
}

pub(crate) struct AnyAgent(Arc<dyn DynAgent>);

impl Clone for AnyAgent {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl AnyAgent {
    pub fn downcast<T: ResolveMapping>(self) -> Result<Agent<T>, AgentError> {
        self.0
            .as_any()
            .downcast_ref::<Agent<T>>()
            .cloned()
            .ok_or(AgentError::Downcast)
    }
}

impl<T: ResolveMapping> From<Agent<T>> for AnyAgent {
    fn from(value: Agent<T>) -> Self {
        AnyAgent(Arc::new(value))
    }
}
