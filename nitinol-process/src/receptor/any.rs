use super::Receptor;
use crate::Process;
use std::any::{type_name, Any};
use std::sync::Arc;
use crate::errors::InvalidCast;

pub trait DynRef: 'static + Sync + Send {
    fn as_any(&self) -> &dyn Any;
}

pub(crate) struct AnyRef(Arc<dyn DynRef>);

impl AnyRef {
    pub fn downcast<T: Process>(&self) -> Result<Receptor<T>, InvalidCast> {
        self.0.as_any()
            .downcast_ref::<Receptor<T>>()
            .cloned()
            .ok_or(InvalidCast { to: type_name::<T>() })
    }
}

impl Clone for AnyRef {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl<T: Process> From<Receptor<T>> for AnyRef {
    fn from(value: Receptor<T>) -> Self {
        Self(Arc::new(value))
    }
}
