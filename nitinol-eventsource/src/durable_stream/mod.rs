//! At-least-once event stream that catches up via [`EventStore`] polling and
//! then continues delivering live appends through the same channel.
//!
//! See [`DurableStream`] for the entry point.
//!
//! [`EventStore`]: nitinol_persistence::store::EventStore

mod cursor;
mod poller;
mod proxy;
mod stream;

pub use self::cursor::SequenceCursor;
pub use self::proxy::DurableStreamProxy;
pub use self::stream::{CursorSet, CursorUnset, DurableStream};
