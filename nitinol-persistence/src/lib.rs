pub mod error;
pub mod store;

mod event;
mod event_type;
mod id;
mod query;
mod snapshot;

pub use event::{AppendingEvent, LoadedEvent};
pub use event_type::EventType;
pub use id::{AggregateId, ProjectionId};
pub use query::{AppendOutcome, LoadQuery};
pub use snapshot::PersistedSnapshot;
