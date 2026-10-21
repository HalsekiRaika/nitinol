//! Framework-managed persistent messages (`SystemEvent`).
//!
//! `SystemEvent` is the framework-internal counterpart to the user-facing
//! [`crate::Event`] marker.  It describes messages the framework persists on a
//! stream to coordinate its own machinery (a saga's schedule, consumed-position
//! and lifecycle facts are the current users).  Unlike [`crate::Event`], a `SystemEvent`
//! owns its serialization: there is no user codec involved, so the replay path
//! can decode the markers without going through `Saga::Event`'s codec.
//!
//! The two traits are deliberately independent — neither is a super-trait of
//! the other — and `SystemEvent` is hidden from documentation so consumers are
//! not tempted to implement it.  What the two do share is that each states the
//! one [`EventType`] its rows carry, and that shared statement is what
//! [`DeclaresEventType`](nitinol_contract::DeclaresEventType) names, so one
//! match against a stored identity answers for both families.  Each
//! implementor of `SystemEvent` declares it by hand: the blanket that covers
//! every [`crate::Event`] does not reach a framework record, precisely because
//! no framework record is one.

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
    /// The one identity every row of this record carries.
    ///
    /// An enum implementor is written down under this one path too: which case
    /// a value holds is carried by the bytes [`Self::encode`] writes, and
    /// [`Self::decode`] is what reads it back.
    const EVENT_TYPE: EventType;

    fn encode(&self) -> Bytes;

    fn decode(payload: &[u8]) -> Result<Self, SystemEventDecodeError>;

    /// The row this record is written down as, at `sequence` and `occurred_at`.
    ///
    /// Everything else the row carries is settled by the record itself — the
    /// identity it declares and the bytes it writes — so it builds its own row
    /// rather than the row being assembled again at each place a marker is
    /// appended.  `sequence` and `occurred_at` are the machine's coordinates
    /// and stay the caller's: only the caller knows the position it holds on
    /// the stream.
    ///
    /// Nothing here can fail: a `SystemEvent` owns its serialization.  The
    /// user-event counterpart, [`EventAppending`](crate::EventAppending), goes
    /// through a codec that can refuse, and answers with a `Result`.
    fn appending(&self, sequence: u64, occurred_at: Timestamp) -> AppendingEvent {
        AppendingEvent {
            sequence,
            event_type: Self::EVENT_TYPE,
            payload: self.encode(),
            occurred_at,
        }
    }
}
