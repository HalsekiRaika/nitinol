use std::sync::Arc;
use crate::agent::extension::Extensions;

pub struct Context {
    pub(crate) sequence: i64,
    pub(crate) extension: Arc<Extensions>
}

impl Context {
    pub fn current_sequence(&self) -> i64 {
        self.sequence
    }
    
    pub fn extract<T>(&self) -> Option<T> 
        where T: Clone + Sync + Send + 'static
    {
        self.extension
            .get::<T>()
            .cloned()
    }
}
