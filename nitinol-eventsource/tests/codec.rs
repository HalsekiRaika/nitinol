// Unit tests for Codec<E>, ErasedCodec<E>, blanket impl, and CodecError.
//
// Verified behaviour:
// - Codec<E> can be implemented with associated encode/decode functions
// - encode → decode roundtrip preserves the value
// - blanket impl promotes any Codec<E> to ErasedCodec<E>
// - Arc<dyn ErasedCodec<E>> works via the blanket impl
// - CodecError::Encode is produced when encode fails
// - CodecError::Decode is produced when decode fails (invalid payload)
// - CodecError implements std::error::Error

use std::sync::Arc;

use bytes::Bytes;
use serde::{Deserialize, Serialize};

use nitinol_eventsource::codec::{Codec, ErasedCodec};
use nitinol_eventsource::error::CodecError;

// Fixtures: test event type

#[derive(Debug, PartialEq, Clone, Serialize, Deserialize)]
struct MyEvent {
    value: i32,
    name: String,
}

// Fixtures: JsonCodec — serde_json-backed Codec<E>

/// Codec that uses serde_json for serialisation.
/// Works for any E that implements Serialize + DeserializeOwned.
#[derive(Default)]
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

#[derive(Default)]
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

// Helper: type-level assertion that a value is Arc<dyn ErasedCodec<E>>

fn assert_erased_codec<E: Send + Sync + 'static>(_: &Arc<dyn ErasedCodec<E>>) {}

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
    // Qualified syntax required: the blanket impl `ErasedCodec<E> for C: Codec<E>`
    // adds an `encode` method to JsonCodec, causing E0034 without disambiguation.
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

// blanket impl: Codec<E> → ErasedCodec<E>

/// Arc::new(JsonCodec) can be coerced to Arc<dyn ErasedCodec<MyEvent>>
/// via the blanket impl.
#[test]
fn codec_impl_can_be_upcast_to_arc_dyn_erased_codec() {
    // Given / When
    let codec: Arc<dyn ErasedCodec<MyEvent>> = Arc::new(JsonCodec);

    // Then: type-level assertion (compile-time proof)
    assert_erased_codec(&codec);
}

/// ErasedCodec::encode via blanket impl succeeds and returns non-empty bytes.
#[test]
fn erased_codec_encode_via_blanket_impl_succeeds() {
    // Given
    let codec: Arc<dyn ErasedCodec<MyEvent>> = Arc::new(JsonCodec);
    let event = MyEvent {
        value: 5,
        name: "erased".to_string(),
    };

    // When
    let result = codec.encode(&event);

    // Then
    assert!(
        result.is_ok(),
        "ErasedCodec encode must succeed via blanket impl"
    );
    assert!(
        !result
            .expect("result is Ok, as asserted immediately above")
            .is_empty(),
        "encoded bytes must not be empty for a non-trivial event"
    );
}

/// ErasedCodec roundtrip via blanket impl preserves the value.
#[test]
fn erased_codec_roundtrip_via_blanket_impl_preserves_value() {
    // Given
    let codec: Arc<dyn ErasedCodec<MyEvent>> = Arc::new(JsonCodec);
    let original = MyEvent {
        value: 123,
        name: "erased-roundtrip".to_string(),
    };

    // When
    let bytes = codec
        .encode(&original)
        .expect("ErasedCodec encode must succeed");
    let decoded = codec
        .decode(&bytes)
        .expect("ErasedCodec decode must succeed");

    // Then
    assert_eq!(
        decoded, original,
        "ErasedCodec roundtrip must preserve the event value"
    );
}

// CodecError: error variant wrapping

/// Decode with an invalid JSON payload produces CodecError::Decode.
#[test]
fn erased_codec_decode_invalid_payload_produces_codec_error_decode() {
    // Given: payload that is not valid JSON for MyEvent
    let codec: Arc<dyn ErasedCodec<MyEvent>> = Arc::new(JsonCodec);
    let bad_payload = b"not valid json at all";

    // When
    let result = codec.decode(bad_payload);

    // Then
    match result {
        Err(CodecError::Decode(_)) => {}
        Ok(_) => panic!("decode of invalid payload must not succeed"),
        Err(CodecError::Encode(_)) => panic!("wrong error variant: expected Decode, got Encode"),
    }
}

/// Decode of a structurally valid JSON but wrong shape produces CodecError::Decode.
#[test]
fn erased_codec_decode_wrong_shape_json_produces_codec_error_decode() {
    // Given: valid JSON but missing required fields of MyEvent
    let codec: Arc<dyn ErasedCodec<MyEvent>> = Arc::new(JsonCodec);
    let wrong_shape = br#"{"unexpected_field": true}"#;

    // When
    let result = codec.decode(wrong_shape);

    // Then
    match result {
        Err(CodecError::Decode(_)) => {}
        Ok(_) => panic!("decode of wrong-shape JSON must not succeed"),
        Err(CodecError::Encode(_)) => panic!("wrong error variant: expected Decode, got Encode"),
    }
}

/// AlwaysFailCodec::encode error is wrapped as CodecError::Encode.
#[test]
fn erased_codec_encode_failure_produces_codec_error_encode() {
    // Given
    let codec: Arc<dyn ErasedCodec<MyEvent>> = Arc::new(AlwaysFailCodec);
    let event = MyEvent {
        value: 1,
        name: "x".to_string(),
    };

    // When
    let result = codec.encode(&event);

    // Then
    match result {
        Err(CodecError::Encode(_)) => {}
        Ok(_) => panic!("AlwaysFailCodec encode must not succeed"),
        Err(CodecError::Decode(_)) => panic!("wrong error variant: expected Encode, got Decode"),
    }
}

/// AlwaysFailCodec::decode error is wrapped as CodecError::Decode.
#[test]
fn erased_codec_decode_failure_produces_codec_error_decode_custom() {
    // Given
    let codec: Arc<dyn ErasedCodec<MyEvent>> = Arc::new(AlwaysFailCodec);

    // When
    let result = codec.decode(b"anything");

    // Then
    match result {
        Err(CodecError::Decode(_)) => {}
        Ok(_) => panic!("AlwaysFailCodec decode must not succeed"),
        Err(CodecError::Encode(_)) => panic!("wrong error variant: expected Decode, got Encode"),
    }
}

// CodecError: implements std::error::Error

/// CodecError::Encode implements std::error::Error (Display + Debug + Send + Sync).
#[test]
fn codec_error_encode_implements_std_error() {
    // Given
    let inner: Box<dyn std::error::Error + Send + Sync> = Box::new(ForcedError);
    let error = CodecError::Encode(inner);

    // When / Then: format it to confirm Display and Error trait are satisfied
    let _ = format!("{}", error);
    let _ = format!("{:?}", error);

    // Ensure it can be used as dyn Error (type-level assertion)
    let _: &dyn std::error::Error = &error;
}

/// CodecError::Decode implements std::error::Error.
#[test]
fn codec_error_decode_implements_std_error() {
    // Given
    let inner: Box<dyn std::error::Error + Send + Sync> = Box::new(ForcedError);
    let error = CodecError::Decode(inner);

    // When / Then
    let _ = format!("{}", error);
    let _ = format!("{:?}", error);
    let _: &dyn std::error::Error = &error;
}
