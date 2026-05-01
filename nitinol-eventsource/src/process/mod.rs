mod codec;
pub(crate) mod aggregate_process;
mod props;
mod proxy;

pub use self::codec::{DecodeError, EncodeError, EventCodec};
pub use self::props::AggregateProps;
pub use self::proxy::AggregateProxy;
