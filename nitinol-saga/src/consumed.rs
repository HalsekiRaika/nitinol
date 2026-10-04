//! `ConsumedPositionEvent` — the position of an upstream record the saga has
//! consumed, recorded as a fact on the saga's own stream.
//!
//! Modelled on the outbox and schedule declarations: one reserved
//! [`EventType`] (`nitinol.saga.consumed_position`) every position is written
//! down as, with a prost `oneof` codec that owns its own serialization.
//!
//! # What makes an input consumed
//!
//! Most inputs are consumed by a `handle` decision: the position is
//! committed by the append that closes out every decision the input
//! produced — the last one, when one `handle` call reaches the store as more
//! than one atomic batch — so it is never durable before all of them are: a
//! batch that never landed leaves no position behind, and the input is
//! handled again on redelivery.
//!
//! An input can also be consumed without ever producing a `handle` decision:
//! a record that arrives while an earlier record from the same correlation is
//! already draining to a dead letter, and a record for which `handle` returns
//! `Err`, are each settled by a dead-letter append that carries the same
//! position instead.  That append landing is what makes the input consumed in
//! these cases — see `Saga::handle` for the rule these settle paths are
//! exceptions to.
//!
//! A record whose upstream bytes fail to decode writes no position at all: it
//! is reported and skipped, and the subscription's cursor advances past it
//! without anything durable being recorded.  A decode failure is deterministic
//! and permanent, so re-reading the record after a restart repeats the report
//! rather than the decision.
//!
//! Either way, de-duplication of the input is moved off the user's `handle`
//! and onto the framework, and nothing here decides *when* to write a
//! position on its own; the interpreter or the dead-letter path hands one to
//! whichever append site is closing out the input.
//!
//! # Why a position per declared source
//!
//! A position names both the upstream stream it was read from *and* the
//! declared source — the event type — it settled.  The stream key alone is not
//! enough: two declared sources can live on one stream (an aggregate emitting
//! two event types, a saga observing another saga's stream), and keyed by the
//! stream alone the higher of the two positions would cover the other source's
//! records, so one source's progress would silently drop the other's input.
//!
//! A saga that observes several sources therefore holds one position per
//! (stream, source) pair, and one source's progress can never mask a record of
//! another.

use std::collections::HashMap;

use bytes::Bytes;
use nitinol_eventsource::{SequenceCursor, SystemEvent, SystemEventDecodeError};
use nitinol_persistence::{
    is_within_reserved_namespace, reserved_event, AggregateId, AppendingEvent, EventType,
};

use crate::observability::report_system_event;

mod proto {
    include!(concat!(
        env!("OUT_DIR"),
        "/nitinol.saga.consumed_position.rs"
    ));
}

use self::proto::consumed_position_marker::Kind;
use self::proto::{Consumed, ConsumedPositionMarker};

reserved_event! {
    /// The one identity every consumed-position record is written down as.
    ///
    /// What a binary linking this crate collects for the position, so that a
    /// load of a saga's own stream restores it rather than failing on it.
    pub(crate) const CONSUMED_POSITION_MARKER {
        family = "nitinol.saga",
        type_name = "consumed_position",
        declared_by = ConsumedPositionEvent,
    }

    // The one fact a consumed-position record states.  A stored row names the
    // declaration, so the fact lives in the payload's `oneof`; this is the name
    // that `oneof` is reported under.
    arm_type = ConsumedPositionEvent;
    ConsumedPositionEvent { .. } => pub(crate) CONSUMED_ARM = "consumed",
}

#[derive(Debug, thiserror::Error)]
#[error("consumed position payload carried no `oneof` kind")]
struct MissingConsumedKind;

#[derive(Debug, thiserror::Error)]
#[error("consumed position `source` must not be empty or within the reserved namespace")]
struct InvalidConsumedSource;

/// Which declared source a consumed position settled.
///
/// The framework always writes [`PositionSource::Declared`]: a record reaches
/// `handle` as one variant of the saga's alphabet, so the source it belongs to
/// is known at the moment the position is built.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum PositionSource {
    /// The declared source, named by the canonical path of its event type.
    Declared(String),
    /// A row written before positions named a source.
    ///
    /// Such a row cannot say which source it settled, so it is read as covering
    /// every source of its stream.  Covering is the safe reading: treating it
    /// as covering nothing would re-deliver input whose decision is already
    /// durable.
    WholeStream,
}

impl PositionSource {
    /// The declared source an event type names.
    pub(crate) fn of(event_type: EventType) -> Self {
        // `Display` on `EventType` is the canonical wire string — the same one
        // `to_path().to_string()` produces — so both sides of the column agree
        // on the encoding without this module restating it.
        Self::Declared(event_type.to_string())
    }

    /// The wire form: the canonical path, or the empty string for a row that
    /// names no source.
    fn as_wire(&self) -> &str {
        match self {
            Self::Declared(path) => path,
            Self::WholeStream => "",
        }
    }

    /// Read the wire form back.  Proto3 decodes an unset field as the empty
    /// string, which is exactly the shape a row written before sources were
    /// named has.
    fn from_wire(wire: String) -> Self {
        if wire.is_empty() {
            Self::WholeStream
        } else {
            Self::Declared(wire)
        }
    }
}

/// One upstream record the saga has consumed: the stream it was read from, the
/// declared source it belongs to, and its sequence in that stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ConsumedPositionEvent {
    pub(crate) source: AggregateId,
    pub(crate) source_event_type: PositionSource,
    pub(crate) sequence: u64,
}

impl SystemEvent for ConsumedPositionEvent {
    const EVENT_TYPE: EventType = CONSUMED_POSITION_MARKER;

    fn encode(&self) -> Bytes {
        let marker = ConsumedPositionMarker {
            kind: Some(Kind::Consumed(Consumed {
                source: self.source.as_str().to_owned(),
                sequence: self.sequence,
                source_event_type: self.source_event_type.as_wire().to_owned(),
            })),
        };
        Bytes::from(prost::Message::encode_to_vec(&marker))
    }

    fn decode(payload: &[u8]) -> Result<Self, SystemEventDecodeError> {
        let marker = <ConsumedPositionMarker as prost::Message>::decode(payload)
            .map_err(SystemEventDecodeError::new)?;
        match marker.kind {
            Some(Kind::Consumed(m)) => {
                // `source` is proto3, so an unset field decodes as the empty
                // string rather than a missing `oneof` — that case is caught
                // above.  An empty or reserved-namespace source can only be a
                // corrupted row: `encode` above only ever writes a position
                // built from an already-valid `AggregateId`, and the
                // framework never mints one of those from either.  Reading
                // it as sound would fold in a position that covers a record
                // no saga ever decided from, or reach `AggregateId::new`'s
                // reserved-namespace guard and panic replay outright instead
                // of reporting a decode error the caller can skip.
                if m.source.is_empty() || is_within_reserved_namespace(&m.source) {
                    return Err(SystemEventDecodeError::new(InvalidConsumedSource));
                }
                Ok(Self {
                    source: AggregateId::new(m.source),
                    // An empty `source_event_type`, unlike an empty `source`,
                    // is a row the framework really did write — before
                    // positions named a source — so it is read rather than
                    // refused.
                    source_event_type: PositionSource::from_wire(m.source_event_type),
                    sequence: m.sequence,
                })
            }
            // A row with no `oneof` says nothing about what was consumed.
            // Reading it as a default would claim the empty source at sequence
            // 0 — a position that covers a record no saga ever decided from.
            None => Err(SystemEventDecodeError::new(MissingConsumedKind)),
        }
    }
}

// A stored identity is matched against this record the same way it is matched
// against a domain event.  Written out by hand because the blanket that covers
// every `Event` cannot reach a `SystemEvent`: the two traits are independent
// and nothing implements both, which is also why this does not conflict with
// it.  The trait is named by path rather than imported so `EVENT_TYPE` in this
// module keeps resolving to the one `SystemEvent` states.
impl nitinol_eventsource::DeclaresEventType for ConsumedPositionEvent {
    const EVENT_TYPE: EventType = <Self as SystemEvent>::EVENT_TYPE;
}

/// How far each declared source of each upstream stream has been consumed.
///
/// Rebuilt by folding the saga's own stream and advanced by every batch that
/// lands, so the running incarnation and a replayed one answer the same
/// question the same way.
#[derive(Default)]
pub(crate) struct ConsumedPositions(HashMap<String, HashMap<PositionSource, u64>>);

impl ConsumedPositions {
    /// Record that `sequence` of `source` on stream `stream` is consumed.
    ///
    /// A position moves forward only: the stream may hold a later row naming a
    /// lower sequence — an input redelivered while an earlier incarnation was
    /// still writing — and letting it win would re-open inputs whose decisions
    /// are already durable.
    pub(crate) fn record(&mut self, stream: &str, source: PositionSource, sequence: u64) {
        self.0
            .entry(stream.to_owned())
            .or_default()
            .entry(source)
            .and_modify(|highest| *highest = (*highest).max(sequence))
            .or_insert(sequence);
    }

    /// Has `sequence` of the declared source `source` on stream `stream`
    /// already been consumed?
    ///
    /// Answers for the named source alone, plus whatever a source-less row of
    /// the same stream covers: a position of *another* declared source says
    /// nothing about this record, and neither does a position of another
    /// stream.
    pub(crate) fn covers(&self, stream: &str, source: &PositionSource, sequence: u64) -> bool {
        self.highest(stream, source)
            .is_some_and(|highest| highest >= sequence)
    }

    /// The highest sequence of `source` on `stream` that is consumed, or `None`
    /// when nothing of it has been.
    ///
    /// A source-less row of the same stream contributes too: it cannot say
    /// which source it settled, so it covers them all.
    fn highest(&self, stream: &str, source: &PositionSource) -> Option<u64> {
        let of_stream = self.0.get(stream)?;
        let named = of_stream.get(source).copied();
        let whole_stream = of_stream.get(&PositionSource::WholeStream).copied();
        match (named, whole_stream) {
            (None, None) => None,
            (named, whole_stream) => Some(named.unwrap_or(0).max(whole_stream.unwrap_or(0))),
        }
    }

    /// Where a subscription over the declared source `source` configured with
    /// `cursor` should resume.
    ///
    /// A stream subscription reads one source of one stream, so its position is
    /// the one this saga holds for that pair — which is what frees a restart
    /// from re-reading the whole upstream history.  The configured `after` is a
    /// floor rather than a starting point: it says "do not deliver these", and
    /// a consumed position is a second reason to skip, never a reason to start
    /// delivering.
    ///
    /// A `Global` cursor is left exactly as configured: its dimension is a
    /// store-wide ordering that a per-source sequence cannot be translated
    /// into, and guessing one would skip records nobody consumed.  There,
    /// de-duplication is the position's job rather than the cursor's.
    pub(crate) fn resume(&self, cursor: SequenceCursor, source: &PositionSource) -> SequenceCursor {
        match cursor {
            SequenceCursor::Stream { key, after } => {
                let consumed = self.highest(&key, source).unwrap_or(0);
                SequenceCursor::Stream {
                    after: after.max(consumed),
                    key,
                }
            }
            SequenceCursor::Global { after } => SequenceCursor::Global { after },
        }
    }
}

/// Add the position of the upstream record this append settles to the batch
/// being built, and answer with the position it now carries.
///
/// Called from two kinds of call site: the interpreter, closing out a
/// `handle` decision, and the dead-letter path, settling a record that never
/// produced one (a record dropped while draining, a `handle` error, or a
/// failure the policy decided to ignore). One `handle`
/// invocation's decisions can reach the store as more than one atomic
/// append — a `CancelSchedule` or `End` is a hard boundary the
/// `Persist × Persist` fold does not cross — so on that call site the
/// position must go on whichever append lands last for the input, never an
/// earlier one: recording it any sooner would let a redelivery it now blocks
/// skip a decision this same input still owes, because a later append in the
/// chain never landed.  Each call site arranges this by only ever handing
/// `input_position` to the append that is closing out the input.
///
/// Nothing is added when there is no upstream input to consume, or when the
/// position is already durable — a retried delivery whose earlier attempt
/// already landed this exact position.
pub(crate) fn append_consumed_position(
    appending: &mut Vec<AppendingEvent>,
    input_position: Option<&ConsumedPositionEvent>,
    consumed: &ConsumedPositions,
    next_seq: &mut u64,
    now: jiff::Timestamp,
) -> Option<ConsumedPositionEvent> {
    let position = input_position?;
    if consumed.covers(
        position.source.as_str(),
        &position.source_event_type,
        position.sequence,
    ) {
        return None;
    }
    *next_seq += 1;
    appending.push(position.appending(*next_seq, now));
    Some(position.clone())
}

/// Fold a position that just landed into the in-memory record and report it.
///
/// Call this only after the append carrying `recorded` has actually
/// succeeded.  Shared by every append site that can close out an input — a
/// `Persist` batch, a `CancelSchedule` marker, the `Ended` marker, a
/// dead-letter append, or the position-only append a policy-ignored failure
/// still writes — so the running incarnation answers `covers` the same way a
/// replayed one does no matter which of them happened to land last.
pub(crate) fn finalize_consumed_position(
    consumed: &mut ConsumedPositions,
    saga_id: &str,
    recorded: Option<ConsumedPositionEvent>,
) {
    if let Some(position) = recorded {
        consumed.record(
            position.source.as_str(),
            position.source_event_type.clone(),
            position.sequence,
        );
        report_system_event!(
            ConsumedPositionEvent::EVENT_TYPE,
            CONSUMED_ARM,
            saga_id,
            source = position.source.as_str(),
            source_event_type = position.source_event_type.as_wire(),
            upstream_sequence = position.sequence,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use nitinol_persistence::{Family, TypeName};

    const ALPHA: EventType = EventType::new(Family::new("consumed_test"), TypeName::new("Alpha"));
    const BETA: EventType = EventType::new(Family::new("consumed_test"), TypeName::new("Beta"));

    fn alpha() -> PositionSource {
        PositionSource::of(ALPHA)
    }

    fn beta() -> PositionSource {
        PositionSource::of(BETA)
    }

    fn position(source: &str, sequence: u64) -> ConsumedPositionEvent {
        ConsumedPositionEvent {
            source: AggregateId::new(source),
            source_event_type: alpha(),
            sequence,
        }
    }

    #[test]
    fn consumed_position_reserves_a_single_event_type() {
        assert_eq!(
            ConsumedPositionEvent::EVENT_TYPE,
            CONSUMED_POSITION_MARKER,
            "the record is written down under the one identity it declares"
        );
        assert_eq!(
            CONSUMED_POSITION_MARKER.to_string(),
            "nitinol.saga.consumed_position"
        );
    }

    /// The arm a record reports is the name of the `oneof` its payload carries.
    /// The column no longer distinguishes them, so this is the whole of what an
    /// operator has to tell one framework record from another.
    #[test]
    fn the_record_reports_the_arm_its_payload_carries() {
        assert_eq!(position("order-stream", 7).arm(), "consumed");
    }

    #[test]
    fn a_position_round_trips_through_its_codec() {
        let written = position("order-stream", 7);

        match ConsumedPositionEvent::decode(&written.encode()) {
            Ok(read) => assert_eq!(
                read, written,
                "the stream, the declared source and the sequence must all \
                 survive the round trip; a position that lost its source would \
                 cover every source of its stream"
            ),
            Err(e) => panic!("decode failed: {e}"),
        }
    }

    /// The declared source travels as the canonical path of its event type, so
    /// a reader that has only the wire string can still tell two sources of one
    /// stream apart.
    #[test]
    fn the_declared_source_travels_as_its_canonical_path() {
        assert_eq!(
            PositionSource::of(ALPHA).as_wire(),
            ALPHA.to_path().to_string(),
            "the wire form must be the event type's canonical path"
        );
    }

    /// A payload with no `oneof` must not read as a position of the empty
    /// source at sequence 0 — that position would cover a record the saga never
    /// decided from.
    #[test]
    fn decode_reports_error_for_a_payload_missing_the_oneof_kind() {
        assert!(ConsumedPositionEvent::decode(&[]).is_err());
    }

    /// `source` is proto3, so an unset field decodes as `""` rather than a
    /// missing `oneof` — that case is `decode_reports_error_for_a_payload_
    /// missing_the_oneof_kind` above.  An empty source must not read as a
    /// position of the empty source at sequence N: no saga ever decided from
    /// it, and it would cover records nobody consumed.
    #[test]
    fn decode_reports_error_for_an_empty_source_instead_of_a_default() {
        let marker = ConsumedPositionMarker {
            kind: Some(Kind::Consumed(Consumed {
                source: String::new(),
                sequence: 5,
                source_event_type: ALPHA.to_string(),
            })),
        };
        let payload = Bytes::from(prost::Message::encode_to_vec(&marker));

        assert!(
            ConsumedPositionEvent::decode(&payload).is_err(),
            "an empty source is proto3's default for an unset field, not a \
             real position; decode must not read it as one"
        );
    }

    /// An empty `source_event_type`, unlike an empty `source`, is a row the
    /// framework itself wrote before positions named a source.  It reads as a
    /// position that covers the whole stream rather than as a decode error.
    #[test]
    fn a_row_naming_no_source_reads_as_covering_the_whole_stream() {
        let marker = ConsumedPositionMarker {
            kind: Some(Kind::Consumed(Consumed {
                source: "order-stream".to_owned(),
                sequence: 5,
                source_event_type: String::new(),
            })),
        };
        let payload = Bytes::from(prost::Message::encode_to_vec(&marker));

        match ConsumedPositionEvent::decode(&payload) {
            Ok(read) => assert_eq!(
                read.source_event_type,
                PositionSource::WholeStream,
                "a row from before sources were named must be readable, and \
                 what it covers is the whole stream"
            ),
            Err(e) => panic!("decode failed: {e}"),
        }
    }

    /// `encode` only ever writes a position built from an already-valid
    /// `AggregateId`, so a stored row naming a reserved-namespace source can
    /// only be corrupted data.  `AggregateId::new` panics on such a name;
    /// `decode` must catch it first and report a decode error instead of
    /// letting that panic take down replay.
    #[test]
    fn decode_reports_error_for_a_reserved_source_instead_of_panicking() {
        let marker = ConsumedPositionMarker {
            kind: Some(Kind::Consumed(Consumed {
                source: "nitinol.saga.poisoned".to_owned(),
                sequence: 5,
                source_event_type: ALPHA.to_string(),
            })),
        };
        let payload = Bytes::from(prost::Message::encode_to_vec(&marker));

        assert!(
            ConsumedPositionEvent::decode(&payload).is_err(),
            "a reserved-namespace source can only be a corrupted row; decode \
             must reject it rather than let `AggregateId::new` panic"
        );
    }

    #[test]
    fn a_recorded_position_covers_itself_and_everything_below_it() {
        let mut positions = ConsumedPositions::default();
        positions.record("order-stream", alpha(), 3);

        assert!(positions.covers("order-stream", &alpha(), 1));
        assert!(positions.covers("order-stream", &alpha(), 3));
        assert!(
            !positions.covers("order-stream", &alpha(), 4),
            "the next record is not consumed until its own decision is durable"
        );
    }

    #[test]
    fn a_later_lower_position_does_not_move_the_position_backwards() {
        let mut positions = ConsumedPositions::default();
        positions.record("order-stream", alpha(), 3);
        positions.record("order-stream", alpha(), 1);

        assert!(
            positions.covers("order-stream", &alpha(), 3),
            "a stale row must not re-open an input whose decision is already durable"
        );
    }

    #[test]
    fn a_position_of_one_stream_covers_no_other() {
        let mut positions = ConsumedPositions::default();
        positions.record("source-a", alpha(), 3);

        assert!(
            !positions.covers("source-b", &alpha(), 1),
            "a single position shared by every stream would swallow records of \
             streams this saga has never consumed from"
        );
    }

    /// The invariant the per-source key exists for: two declared sources on one
    /// stream key must not cover each other.
    #[test]
    fn a_position_of_one_declared_source_covers_no_other_source_of_the_same_stream() {
        let mut positions = ConsumedPositions::default();
        positions.record("shared", alpha(), 3);

        assert!(
            !positions.covers("shared", &beta(), 2),
            "keyed by the stream alone, source A's progress would drop source \
             B's record at a lower sequence — input no incarnation ever consumed"
        );
    }

    /// A row that names no source cannot say which source it settled, so it
    /// covers them all.  The alternative — covering nothing — would re-deliver
    /// input whose decision is already durable.
    #[test]
    fn a_source_less_position_covers_every_source_of_its_stream() {
        let mut positions = ConsumedPositions::default();
        positions.record("shared", PositionSource::WholeStream, 3);

        assert!(positions.covers("shared", &alpha(), 3));
        assert!(positions.covers("shared", &beta(), 2));
        assert!(
            !positions.covers("shared", &beta(), 4),
            "it covers up to its own sequence and no further"
        );
    }

    #[test]
    fn a_stream_subscription_resumes_past_the_consumed_position() {
        let mut positions = ConsumedPositions::default();
        positions.record("order-stream", alpha(), 3);

        let resumed = positions.resume(
            SequenceCursor::Stream {
                key: "order-stream".to_owned(),
                after: 0,
            },
            &alpha(),
        );

        assert!(
            matches!(resumed, SequenceCursor::Stream { ref key, after }
                if key == "order-stream" && after == 3),
            "the subscription must not ask upstream for a record it has already \
             decided from; got {resumed:?}"
        );
    }

    /// Two declared sources on one stream resume independently: one source's
    /// progress must not move the other's subscription past records it never
    /// consumed.
    #[test]
    fn each_declared_source_resumes_from_its_own_position() {
        let mut positions = ConsumedPositions::default();
        positions.record("shared", alpha(), 3);

        let resumed = positions.resume(
            SequenceCursor::Stream {
                key: "shared".to_owned(),
                after: 0,
            },
            &beta(),
        );

        assert!(
            matches!(resumed, SequenceCursor::Stream { after, .. } if after == 0),
            "source B has consumed nothing, so its subscription starts where it \
             was configured; got {resumed:?}"
        );
    }

    #[test]
    fn the_configured_floor_survives_a_lower_consumed_position() {
        let mut positions = ConsumedPositions::default();
        positions.record("order-stream", alpha(), 1);

        let resumed = positions.resume(
            SequenceCursor::Stream {
                key: "order-stream".to_owned(),
                after: 3,
            },
            &alpha(),
        );

        assert!(
            matches!(resumed, SequenceCursor::Stream { after, .. } if after == 3),
            "`after` says \"do not deliver these\"; a lower consumed position is \
             never a reason to start delivering them; got {resumed:?}"
        );
    }

    #[test]
    fn a_subscription_over_an_unconsumed_source_keeps_its_configured_position() {
        let mut positions = ConsumedPositions::default();
        positions.record("other-stream", alpha(), 9);

        let resumed = positions.resume(
            SequenceCursor::Stream {
                key: "order-stream".to_owned(),
                after: 2,
            },
            &alpha(),
        );

        assert!(
            matches!(resumed, SequenceCursor::Stream { after, .. } if after == 2),
            "another source's progress must not move this subscription; got {resumed:?}"
        );
    }

    #[test]
    fn a_global_subscription_keeps_its_configured_cursor() {
        let mut positions = ConsumedPositions::default();
        positions.record("order-stream", alpha(), 3);

        let resumed = positions.resume(SequenceCursor::Global { after: 0 }, &alpha());

        assert!(
            matches!(resumed, SequenceCursor::Global { after } if after == 0),
            "a per-source sequence cannot be translated into a global ordering, \
             so guessing one would skip records nobody consumed; got {resumed:?}"
        );
    }
}
