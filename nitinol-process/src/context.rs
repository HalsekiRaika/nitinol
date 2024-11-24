use std::sync::atomic::{AtomicBool, Ordering};
use crate::registry::Registry;

pub trait ProcessContext: 'static + Sync + Send {
    fn sequence(&self) -> i64;
    fn is_active(&self) -> bool;
    fn poison_pill(&mut self);
    fn registry(&self) -> &Registry;
}

pub struct Context {
    pub(crate) sequence: i64,
    pub(crate) is_active: AtomicBool,
    pub(crate) registry: Registry
}

impl Context {
    pub fn new(sequence: i64, registry: Registry) -> Context {
        Self { sequence, is_active: AtomicBool::new(true), registry }
    }
}

impl ProcessContext for Context {
    fn sequence(&self) -> i64 {
        self.sequence
    }

    fn is_active(&self) -> bool {
        self.is_active.load(Ordering::Relaxed)
    }

    fn poison_pill(&mut self) {
        self.is_active.store(false, Ordering::SeqCst);
    }
    
    fn registry(&self) -> &Registry {
        &self.registry
    }
}
