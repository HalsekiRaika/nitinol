mod aggregate;
pub mod codec;
mod context;
mod decider;
mod effect;
mod event;
mod receive;

pub mod error;
mod process;
pub mod projection;

pub use self::aggregate::{Aggregate, Snapshotable};
pub use self::codec::{Codec, ErasedCodec, EventSourceSystem, EventSourceSystemBuilder};
pub use self::context::Context;
pub use self::decider::Decider;
pub use self::effect::{Effect, SideEffect, SideEffectError};
pub use self::error::{CodecError, EffectExecutionError};
pub use self::event::Event;
pub use self::receive::Receive;

pub use self::process::{AggregateProps, AggregateProxy};
pub use self::process::{EventPersistor, EventPersistorRef, SnapshotPersistor, SnapshotPersistorRef};
pub use self::error::{AskError, ExecError, TellError, PersistorUnreachableKind};

pub use self::projection::{EventEnvelope, ProjectionContext, Projector, ProjectorProps};
