use super::Ref;
use crate::Process;
use std::any::{type_name, Any};
use std::sync::Arc;

pub trait DynRef: 'static + Sync + Send {
    fn as_any(&self) -> &dyn Any;
}

pub(crate) struct AnyRef(Arc<dyn DynRef>);

impl AnyRef {
    pub fn downcast<T: Process>(&self) -> Result<Ref<T>, InvalidCast> {
        self.0.as_any()
            .downcast_ref::<Ref<T>>()
            .cloned()
            .ok_or(InvalidCast { to: type_name::<T>() })
    }
}

impl Clone for AnyRef {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl<T: Process> From<Ref<T>> for AnyRef {
    fn from(value: Ref<T>) -> Self {
        Self(Arc::new(value))
    }
}

#[derive(Debug, thiserror::Error)]
#[error("Invalid cast to {to}")]
pub struct InvalidCast {
    to: &'static str
}