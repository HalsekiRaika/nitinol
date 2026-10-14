//! `Codec<E>` and the error family the framework classifies its failures into.
//!
//! Verified behaviour:
//! - `Codec<E>` can be implemented with associated `encode` / `decode` functions
//! - `encode` → `decode` round-trip preserves the value
//! - a failure of the codec an event names is reported as
//!   `PersistError::Codec(CodecError::Encode(..))`, and is permanent
//!
//! The classification above used to be observed on the blanket impl that
//! promoted a `Codec<E>` into a type-erased handle. There is no such handle any
//! more — an event names its codec and the framework calls it statically — so
//! the same claim is now made where a caller can actually see it: the answer
//! `ask` gives when the named codec refuses to encode.

use bytes::Bytes;
use serde::{Deserialize, Serialize};

use nitinol_eventsource::codec::Codec;
use nitinol_eventsource::error::{AskError, CodecError, PersistError, Retryability};
use nitinol_eventsource::{Aggregate, AggregateProps, Decider, Decision, Event};
use nitinol_persistence::store::{InMemoryEventStore, Journal};
use nitinol_persistence::{AggregateId, EventType, Family, TypeName};
use nitinol_runtime::ProcessSystem;

// Fixtures: test event type

#[derive(Debug, PartialEq, Clone, Serialize, Deserialize)]
struct MyEvent {
    value: i32,
    name: String,
}

// Fixtures: JsonCodec — serde_json-backed Codec<E>

/// Codec that uses serde_json for serialisation.
/// Works for any E that implements Serialize + DeserializeOwned.
///
/// Never constructed: `Codec`'s methods are associated functions, so a codec is
/// reached through its type rather than through a value.
struct JsonCodec;

impl<E: Serialize + for<'de> Deserialize<'de>> Codec<E> for JsonCodec {
    type Error = serde_json::Error;

    fn encode(event: &E) -> Result<Bytes, Self::Error> {
        serde_json::to_vec(event).map(Bytes::from)
    }

    fn decode(payload: &[u8]) -> Result<E, Self::Error> {
        serde_json::from_slice(payload)
    }
}

// Fixtures: AlwaysFailCodec — produces errors on every call

struct AlwaysFailCodec;

#[derive(Debug, thiserror::Error)]
#[error("forced codec failure")]
struct ForcedError;

impl Codec<MyEvent> for AlwaysFailCodec {
    type Error = ForcedError;

    fn encode(_event: &MyEvent) -> Result<Bytes, Self::Error> {
        Err(ForcedError)
    }

    fn decode(_payload: &[u8]) -> Result<MyEvent, Self::Error> {
        Err(ForcedError)
    }
}

// Codec<E>: associated functions compile and return the correct types

/// Codec::encode returns Ok(Bytes) for a valid event.
#[test]
fn codec_encode_returns_ok_bytes_for_valid_event() {
    // Given
    let event = MyEvent {
        value: 42,
        name: "hello".to_string(),
    };

    // When
    // Qualified syntax required: `JsonCodec` implements `Codec<E>` for every
    // serialisable `E`, so the subject has to be named.
    let result = <JsonCodec as Codec<MyEvent>>::encode(&event);

    // Then
    assert!(
        result.is_ok(),
        "encode must succeed for a valid, serialisable event"
    );
}

/// Codec::decode returns Ok(E) for a valid payload produced by encode.
#[test]
fn codec_decode_returns_ok_for_payload_from_encode() {
    // Given
    let event = MyEvent {
        value: 7,
        name: "world".to_string(),
    };
    let bytes = <JsonCodec as Codec<MyEvent>>::encode(&event).expect("encode must succeed");

    // When
    let result: Result<MyEvent, _> = <JsonCodec as Codec<MyEvent>>::decode(&bytes);

    // Then
    assert!(
        result.is_ok(),
        "decode must succeed for payload produced by encode"
    );
}

// Codec<E>: encode → decode roundtrip

/// encode followed by decode yields a value equal to the original.
#[test]
fn codec_encode_decode_roundtrip_preserves_value() {
    // Given
    let original = MyEvent {
        value: 99,
        name: "roundtrip".to_string(),
    };

    // When
    let bytes = <JsonCodec as Codec<MyEvent>>::encode(&original).expect("encode must succeed");
    let decoded: MyEvent =
        <JsonCodec as Codec<MyEvent>>::decode(&bytes).expect("decode must succeed");

    // Then
    assert_eq!(
        decoded, original,
        "decoded value must equal the original event"
    );
}

/// Roundtrip over a zero-value event preserves zero.
#[test]
fn codec_roundtrip_zero_value_event_preserved() {
    // Given
    let original = MyEvent {
        value: 0,
        name: String::new(),
    };

    // When
    let bytes = <JsonCodec as Codec<MyEvent>>::encode(&original).expect("encode must succeed");
    let decoded: MyEvent =
        <JsonCodec as Codec<MyEvent>>::decode(&bytes).expect("decode must succeed");

    // Then
    assert_eq!(
        decoded, original,
        "zero-value event must survive roundtrip unchanged"
    );
}

// A failing codec, classified where a caller can see it

/// An event whose named codec refuses every encode, so the only thing the
/// append can report is the codec's failure.
impl Event for MyEvent {
    const EVENT_TYPE: EventType = EventType::new(Family::new("codec"), TypeName::new("MyEvent"));

    type Codec = AlwaysFailCodec;
}

#[derive(Default)]
struct Recorder;

impl Aggregate for Recorder {
    type Event = MyEvent;

    fn apply(&mut self, _event: MyEvent) {}
}

struct Record;

impl Decider<Record> for Recorder {
    type Output = ();
    type Rejection = std::convert::Infallible;

    fn decide(&self, _cmd: Record) -> Decision<MyEvent, (), Self::Rejection> {
        Decision::persist(vec![MyEvent {
            value: 1,
            name: String::from("unwritable"),
        }])
        .output(())
    }
}

/// Given an accepted decision whose event names a codec that always fails,
/// When the command is asked, Then the caller is told the machinery failed to
/// record it — `Persist(Codec(Encode(..)))` — rather than being given a verdict
/// on the command.
#[tokio::test]
async fn a_failing_encode_is_reported_as_a_codec_error_on_the_persist_path() {
    let system = ProcessSystem::new().await;
    let store = Journal::from(InMemoryEventStore::default());

    let proxy = AggregateProps::<Recorder>::new(AggregateId::new("codec-encode-fails"), store)
        .spawn(&system)
        .await;

    let error = proxy
        .ask(Record)
        .await
        .expect_err("an event that cannot be encoded cannot be appended");

    match &error {
        AskError::Persist(PersistError::Codec(CodecError::Encode(_))) => {}
        other => panic!("expected Persist(Codec(Encode(..))), got {other:?}"),
    }

    assert_eq!(
        error.retryability(),
        Retryability::Permanent,
        "encoding this event is deterministic, so re-issuing it fails identically"
    );
}
