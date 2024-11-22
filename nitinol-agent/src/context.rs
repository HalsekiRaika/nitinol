use std::sync::atomic::{AtomicBool, Ordering};

pub struct Context {
    pub(crate) sequence: i64,
    pub(crate) is_active: AtomicBool,
}

impl Context {
    pub fn sequence(&self) -> i64 {
        self.sequence
    }
    
    pub fn is_active(&self) -> bool {
        self.is_active.load(Ordering::Relaxed)
    }
    
    pub fn poison_pill(&mut self) {
        self.is_active.store(false, Ordering::SeqCst);
    }
}