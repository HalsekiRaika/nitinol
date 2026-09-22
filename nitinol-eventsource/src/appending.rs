//! Building the row a user [`Event`] is written down as.
//!
//! The counterpart of [`SystemEvent::appending`](crate::SystemEvent::appending)
//! for the user-facing side: everything a row of a domain event carries beyond
//! its position and instant is settled by the declaration alone — the identity
//! is [`Event::EVENT_TYPE`] and the bytes are whatever the codec it names
//! writes — so the event builds its own row.
//!
//! It is stated here rather than on [`Event`] itself for two reasons. A user
//! codec can refuse, and *what a refusal means* — its classification into
//! [`CodecError`] — is this crate's to decide rather than the contract's. And
//! [`Event`] is what a domain writes by hand: adding to it would change the
//! notation every domain event is declared in, for something no domain
//! implements.

use jiff::Timestamp;
use nitinol_contract::Event;
use nitinol_persistence::AppendingEvent;

use crate::codec;
use crate::error::CodecError;

/// Building the row a user [`Event`] is written down as.
///
/// Implemented for every [`Event`]; there is nothing for a domain to declare.
#[doc(hidden)]
pub trait EventAppending {
    /// The row this event is written down as, at `sequence` and `occurred_at`.
    ///
    /// The bytes are the codec's own, carried verbatim.  A codec that refuses
    /// yields [`CodecError::Encode`] and no row: an encode failure is
    /// deterministic, which is what lets a caller tell it apart from a store
    /// that failed to answer.
    fn appending(
        &self,
        sequence: u64,
        occurred_at: Timestamp,
    ) -> Result<AppendingEvent, CodecError>;
}

impl<E: Event> EventAppending for E {
    fn appending(
        &self,
        sequence: u64,
        occurred_at: Timestamp,
    ) -> Result<AppendingEvent, CodecError> {
        Ok(AppendingEvent {
            sequence,
            event_type: <E as Event>::EVENT_TYPE,
            payload: codec::encode::<E::Codec, E>(self)?,
            occurred_at,
        })
    }
}
