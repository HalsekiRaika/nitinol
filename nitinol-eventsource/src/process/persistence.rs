mod event_persistor;
mod snapshot_persistor;

pub use event_persistor::{EventPersistor, EventPersistorRef};
pub use snapshot_persistor::{SnapshotPersistor, SnapshotPersistorRef};

// Re-export AppendEvents so aggregate_process::run_effect can perform a raw
// ask for fine-grained error mapping (distinguishing handler errors from
// connectivity errors).  Load paths use the public API methods instead.
pub(crate) use event_persistor::AppendEvents;
