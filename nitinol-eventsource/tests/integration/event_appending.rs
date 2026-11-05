//! Building the row a user [`Event`] is written down as.
//!
//! Everything a row of a user event carries is settled by the declaration
//! alone: the identity is [`Event::EVENT_TYPE`] and the bytes are whatever the
//! codec it names writes. So the event builds its own row, rather than the row
//! being assembled again at each place an append is made — three assemblies of
//! one rule are three chances for them to disagree.
//!
//! A user codec can fail, which is the one way this differs from the
//! `SystemEvent` side: a `SystemEvent` owns its serialization and cannot fail,
//! so it answers with a row while an `Event` answers with a `Result`.
//! `event_type_identity.rs` pins that side.

use bytes::Bytes;
use nitinol_eventsource::codec::Codec;
use nitinol_eventsource::error::CodecError;
use nitinol_eventsource::{Event, EventAppending};
use nitinol_persistence::{EventType, Family, TypeName};

#[derive(Clone)]
struct Noted;

/// A codec whose bytes are distinguishable from an empty payload, so a row that
/// lost them is not mistaken for a row that carried them.
struct TextCodec;

impl Codec<Noted> for TextCodec {
    type Error = std::convert::Infallible;

    fn encode(_: &Noted) -> Result<Bytes, Self::Error> {
        Ok(Bytes::from_static(b"noted"))
    }

    fn decode(_: &[u8]) -> Result<Noted, Self::Error> {
        Ok(Noted)
    }
}

impl Event for Noted {
    const EVENT_TYPE: EventType = EventType::new(Family::new("shop.order"), TypeName::new("Noted"));

    type Codec = TextCodec;
}

#[derive(Clone)]
struct Unwritable;

#[derive(Debug, thiserror::Error)]
#[error("this codec refuses to write its event")]
struct RefusedToEncode;

struct RefusingCodec;

impl Codec<Unwritable> for RefusingCodec {
    type Error = RefusedToEncode;

    fn encode(_: &Unwritable) -> Result<Bytes, Self::Error> {
        Err(RefusedToEncode)
    }

    fn decode(_: &[u8]) -> Result<Unwritable, Self::Error> {
        Err(RefusedToEncode)
    }
}

impl Event for Unwritable {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("shop.order"), TypeName::new("Unwritable"));

    type Codec = RefusingCodec;
}

/// Given an event and the position and instant it is being written at, When it
/// builds its row, Then the row carries those two verbatim, the identity its
/// declaration states, and exactly the bytes its own codec wrote.
#[test]
fn the_row_a_user_event_builds_carries_its_declaration_and_its_codecs_bytes() {
    let occurred_at = jiff::Timestamp::from_second(0).expect("epoch must be a valid timestamp");

    let appending = Noted
        .appending(7, occurred_at)
        .expect("an event whose codec succeeds must build its row");

    assert_eq!(
        appending.sequence, 7,
        "the row must take the sequence it was given"
    );
    assert_eq!(
        appending.occurred_at, occurred_at,
        "the row must take the instant it was given rather than read a clock of its own"
    );
    assert_eq!(
        appending.event_type,
        Noted::EVENT_TYPE,
        "the row must be written down under the identity the declaration states"
    );
    assert_eq!(
        appending.payload,
        <TextCodec as Codec<Noted>>::encode(&Noted).expect("direct encode must succeed"),
        "the row must carry the codec's bytes verbatim — nothing wraps or re-encodes them"
    );
}

/// Given an event whose codec refuses it, When it builds its row, Then there is
/// no row and the refusal is reported as an encode failure.
///
/// The classification is what the callers act on: an encode failure is
/// deterministic, so the caller that reports a persist failure and the one that
/// carries on both need it told apart from a store that failed to answer.
#[test]
fn a_codec_that_refuses_yields_an_encode_failure_and_no_row() {
    let occurred_at = jiff::Timestamp::from_second(0).expect("epoch must be a valid timestamp");

    match Unwritable.appending(1, occurred_at) {
        Err(CodecError::Encode(_)) => {}
        Err(other) => {
            panic!("a codec refusing to write must be classified as `Encode`, got {other:?}")
        }
        Ok(_) => panic!("no row may be built from an event its codec refused to write"),
    }
}
