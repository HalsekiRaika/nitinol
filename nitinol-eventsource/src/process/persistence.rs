mod event_persistor;
mod snapshot_persistor;

pub use event_persistor::{EventPersistor, EventPersistorProxy};
pub use snapshot_persistor::{SnapshotPersistor, SnapshotPersistorProxy};

// Re-export AppendEvents so aggregate_process::run_effect can perform a raw
// ask for fine-grained error mapping (distinguishing handler errors from
// connectivity errors).  Load paths use the public API methods instead.
pub(crate) use event_persistor::AppendEvents;
