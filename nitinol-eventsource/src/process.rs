pub(crate) mod aggregate_process;
pub(crate) mod persistence;
mod props;
mod proxy;

pub use self::persistence::{EventPersistor, EventPersistorProxy, SnapshotPersistor, SnapshotPersistorProxy};
pub use self::props::AggregateProps;
pub use self::proxy::AggregateProxy;
