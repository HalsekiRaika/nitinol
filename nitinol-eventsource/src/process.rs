pub(crate) mod aggregate_process;
pub(crate) mod props;
mod proxy;
pub(crate) mod recover;
pub(crate) mod resolve;
mod snapshot_persistor;
mod tell_target;

pub use self::props::AggregateProps;
pub use self::proxy::AggregateProxy;
#[doc(hidden)]
pub use self::recover::SnapshotRecovery;
pub use self::recover::{RecoverOn, RecoverOnContingent};
pub use self::snapshot_persistor::{SnapshotPersistor, SnapshotPersistorProxy};
pub use self::tell_target::AggregateTellTarget;
