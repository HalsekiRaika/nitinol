mod row;
mod lock;

use self::row::Row;
use std::collections::BTreeSet;
use std::sync::Arc;
use tokio::sync::RwLock;

pub struct InMemoryEventStore {
    journal: Arc<RwLock<BTreeSet<Row>>>
}

impl Default for InMemoryEventStore {
    fn default() -> Self {
        Self {
            journal: Arc::new(RwLock::new(BTreeSet::new()))
        }
    }
}
