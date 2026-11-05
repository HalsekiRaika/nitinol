//! `LifecycleEvent` — facts about a saga instance's own life, recorded on its
//! own stream.
//!
//! One reserved [`EventType`] (`nitinol.saga.lifecycle`) every lifecycle fact
//! is written down as, with a prost `oneof` codec that owns its own
//! serialization and carries which fact — so far only `ended` — the row
//! records.
//!
//! `Ended` used to share an envelope with a transport mechanism's transitions,
//! so "this saga terminated" could only be read by a reader that also knew
//! that mechanism's grammar.  It is a fact about the saga's own life, so it
//! has a declaration of its own.

use std::borrow::Borrow;

use bytes::Bytes;
use nitinol_eventsource::{SystemEvent, SystemEventDecodeError};
use nitinol_persistence::store::Journal;
use nitinol_persistence::{reserved_event, EventType};

use crate::consumed::{
    append_consumed_position, finalize_consumed_position, ConsumedPositionEvent, ConsumedPositions,
};
use crate::id::SagaId;
use crate::observability::report_system_event;

mod proto {
    include!(concat!(env!("OUT_DIR"), "/nitinol.saga.lifecycle.rs"));
}

use self::proto::lifecycle_marker::Kind;
use self::proto::{Ended, LifecycleMarker};

reserved_event! {
    /// The one identity every lifecycle fact is written down as.
    ///
    /// What a binary linking this crate collects for the saga's lifecycle, so
    /// that a load of a saga's own stream restores the fact rather than failing
    /// on it.
    pub(crate) const LIFECYCLE_MARKER {
        family = "nitinol.saga",
        type_name = "lifecycle",
        declared_by = LifecycleEvent,
    }

    // The facts a lifecycle row records.  A stored row names the declaration,
    // so the fact lives in the payload's `oneof`; this is the name that `oneof`
    // is reported under.
    arm_type = LifecycleEvent;
    LifecycleEvent::Ended => pub(crate) ENDED_ARM = "ended",
}

#[derive(Debug, thiserror::Error)]
#[error("lifecycle payload carried no `oneof` kind")]
struct MissingLifecycleKind;

/// A fact about the saga instance's own life, persisted on its own stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LifecycleEvent {
    /// The saga terminated.  Its presence is the whole signal: a later spawn
    /// reads it and refuses to wire the upstream subscription.
    Ended,
}

impl SystemEvent for LifecycleEvent {
    const EVENT_TYPE: EventType = LIFECYCLE_MARKER;

    fn encode(&self) -> Bytes {
        let kind = match self {
            LifecycleEvent::Ended => Kind::Ended(Ended {}),
        };
        let marker = LifecycleMarker { kind: Some(kind) };
        Bytes::from(prost::Message::encode_to_vec(&marker))
    }

    fn decode(payload: &[u8]) -> Result<Self, SystemEventDecodeError> {
        let marker = <LifecycleMarker as prost::Message>::decode(payload)
            .map_err(SystemEventDecodeError::new)?;
        match marker.kind {
            Some(Kind::Ended(_)) => Ok(LifecycleEvent::Ended),
            // A row with no `oneof` says nothing about which fact it records.
            None => Err(SystemEventDecodeError::new(MissingLifecycleKind)),
        }
    }
}

// A stored identity is matched against this record the same way it is matched
// against a domain event.  Written out by hand because the blanket that covers
// every `Event` cannot reach a `SystemEvent`: the two traits are independent
// and nothing implements both, which is also why this does not conflict with
// it.  The trait is named by path rather than imported so `EVENT_TYPE` in this
// module keeps resolving to the one `SystemEvent` states.
impl nitinol_eventsource::DeclaresEventType for LifecycleEvent {
    const EVENT_TYPE: EventType = <Self as SystemEvent>::EVENT_TYPE;
}

/// Append the durable `Ended` fact — plus the consumed position, when
/// `input_position` names one this append is closing out — at `*sequence + 1`
/// on the saga's own stream, advancing `*sequence` past every row only on
/// success.  Written when `SagaEffect::End` is interpreted so a later spawn can
/// detect termination and refuse to revive the saga.
///
/// `input_position` is `None` at call sites with no upstream input to consume,
/// and also whenever `End` is not the effect closing out its input — the
/// interpreter's `Sequence` handling only ever hands the position to that one.
///
/// Returns `false` (and does not advance `*sequence`) when the store rejects
/// the append.
pub(crate) async fn append_ended(
    store: &Journal,
    saga_id: &SagaId,
    sequence: &mut u64,
    input_position: Option<&ConsumedPositionEvent>,
    consumed: &mut ConsumedPositions,
) -> bool {
    let now = jiff::Timestamp::now();
    let candidate = *sequence + 1;
    let fact = LifecycleEvent::Ended;
    let mut appending = vec![fact.appending(candidate, now)];
    let mut next_seq = candidate;
    let recorded_position =
        append_consumed_position(&mut appending, input_position, consumed, &mut next_seq, now);

    if let Err(e) = store.append(saga_id.borrow(), appending).await {
        tracing::warn!(error = %e, "saga lifecycle ended fact append failed");
        return false;
    }
    *sequence = next_seq;
    report_system_event!(
        LifecycleEvent::EVENT_TYPE,
        fact.arm(),
        saga_id.as_str(),
        sequence = candidate,
    );
    finalize_consumed_position(consumed, saga_id.as_str(), recorded_position);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifecycle_event_reserves_a_single_event_type() {
        assert_eq!(LifecycleEvent::EVENT_TYPE, LIFECYCLE_MARKER);
        assert_eq!(LIFECYCLE_MARKER.to_string(), "nitinol.saga.lifecycle");
    }

    /// The fact a row states is the name of the `oneof` its payload carries.
    /// The column names the declaration and nothing finer, so this is the whole
    /// of what an operator has to tell one framework record from another.
    #[test]
    fn the_fact_reports_the_arm_its_payload_carries() {
        assert_eq!(LifecycleEvent::Ended.arm(), "ended");
    }

    #[test]
    fn the_ended_fact_round_trips_through_its_codec() {
        match LifecycleEvent::decode(&LifecycleEvent::Ended.encode()) {
            Ok(read) => assert_eq!(read, LifecycleEvent::Ended),
            Err(e) => panic!("decode failed: {e}"),
        }
    }

    /// A payload with no `oneof` must not read as a default fact: the row would
    /// then terminate a saga nothing ever ended.
    #[test]
    fn decode_reports_error_for_a_payload_missing_the_oneof_kind() {
        assert!(LifecycleEvent::decode(&[]).is_err());
    }
}
