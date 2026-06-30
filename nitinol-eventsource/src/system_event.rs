//! Framework-managed persistent messages (`SystemEvent`).
//!
//! `SystemEvent` is the framework-internal counterpart to the user-facing
//! [`crate::Event`] marker.  It describes messages the framework persists on a
//! stream to coordinate its own machinery (the saga outbox markers are the
//! first and only current user).  Unlike [`crate::Event`], a `SystemEvent`
//! owns its serialization: there is no user codec involved, so the replay path
//! can decode the markers without going through `Saga::Event`'s codec.
//!
//! The two traits are deliberately independent — there is no shared super-trait
//! — and `SystemEvent` is hidden from documentation so consumers are not
//! tempted to implement it.

use bytes::Bytes;
use jiff::Timestamp;
use nitinol_persistence::{AppendingEvent, EventType};

use crate::error::SystemEventDecodeError;

/// A framework-managed persistent message with a self-contained codec.
///
/// Each implementor reserves a stable [`EventType`] and defines its own
/// `encode`/`decode` so the framework can write and replay the message without
/// the user's domain codec.  A subsystem groups its implementors into a closed
/// enum to make replay classification exhaustively checked at compile time.
#[doc(hidden)]
pub trait SystemEvent: Sized {
    const EVENT_TYPE: EventType;

    fn encode(&self) -> Bytes;

    fn decode(payload: &[u8]) -> Result<Self, SystemEventDecodeError>;
}

/// Build an [`AppendingEvent`] for a [`SystemEvent`].
///
/// Centralises construction of framework marker events so every write goes
/// through a single, named operation instead of assembling `AppendingEvent`
/// literals at each call site.
#[doc(hidden)]
pub fn appending_system_event<E: SystemEvent>(
    sequence: u64,
    event: &E,
    occurred_at: Timestamp,
) -> AppendingEvent {
    AppendingEvent {
        sequence,
        event_type: E::EVENT_TYPE,
        payload: event.encode(),
        occurred_at,
    }
}
