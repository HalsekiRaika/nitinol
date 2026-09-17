use bytes::Bytes;
use nitinol_eventsource::{codec::Codec, Aggregate, Event, Snapshotable};
use nitinol_persistence::{EventType, Family, TypeName};

// Fixtures

#[derive(Clone)]
struct Incremented;

/// The codec `Incremented` names. Nothing here persists, so the format only has
/// to exist.
struct IncrementedCodec;

impl Codec<Incremented> for IncrementedCodec {
    type Error = std::convert::Infallible;

    fn encode(_: &Incremented) -> Result<Bytes, Self::Error> {
        Ok(Bytes::new())
    }

    fn decode(_: &[u8]) -> Result<Incremented, Self::Error> {
        Ok(Incremented)
    }
}

impl Event for Incremented {
    const EVENT_TYPE: EventType = EventType::new(Family::new(""), TypeName::new("Incremented"));

    type Codec = IncrementedCodec;
}

#[derive(Default)]
struct Counter {
    value: u64,
}

impl Aggregate for Counter {
    type Event = Incremented;

    fn apply(&mut self, _event: Incremented) {
        self.value += 1;
    }
}

/// The codec `Counter` names for its snapshot. As with the event codec, these
/// tests exercise the pure capture / restore functions only.
struct CounterSnapshotCodec;

impl Codec<u64> for CounterSnapshotCodec {
    type Error = std::convert::Infallible;

    fn encode(value: &u64) -> Result<Bytes, Self::Error> {
        Ok(Bytes::copy_from_slice(&value.to_be_bytes()))
    }

    fn decode(payload: &[u8]) -> Result<u64, Self::Error> {
        let mut buf = [0u8; 8];
        buf.copy_from_slice(payload);
        Ok(u64::from_be_bytes(buf))
    }
}

// Snapshot is the raw counter value (u64). `Snapshotable` names the codec that
// writes that value down and defines the pure capture / restore functions; the
// encoding itself is never reached by the assertions below.
impl Snapshotable for Counter {
    type Snapshot = u64;

    type Codec = CounterSnapshotCodec;

    fn capture(&self) -> u64 {
        self.value
    }

    fn restore(snapshot: u64) -> Self {
        Self { value: snapshot }
    }
}

// Event: EVENT_TYPE constant

/// EVENT_TYPE constant carries the correct string value
#[test]
fn event_type_constant_has_correct_string() {
    // Given / When / Then
    assert_eq!(
        Incremented::EVENT_TYPE.to_string(),
        "Incremented",
        "EVENT_TYPE must be the static string 'Incremented'"
    );
}

// Aggregate: Default initial state

/// Counter::default() starts with value 0
#[test]
fn default_counter_has_value_zero() {
    // Given / When
    let counter = Counter::default();

    // Then
    assert_eq!(counter.value, 0, "initial Counter value must be 0");
}

// Aggregate: apply updates state

/// apply(Incremented) increments Counter.value by 1
#[test]
fn apply_incremented_increments_value() {
    // Given
    let mut counter = Counter::default();

    // When
    counter.apply(Incremented);

    // Then
    assert_eq!(
        counter.value, 1,
        "value must be 1 after one apply(Incremented)"
    );
}

/// Applying three events accumulates state correctly
#[test]
fn apply_multiple_events_accumulates_state() {
    // Given
    let mut counter = Counter::default();

    // When
    counter.apply(Incremented);
    counter.apply(Incremented);
    counter.apply(Incremented);

    // Then
    assert_eq!(
        counter.value, 3,
        "value must be 3 after three apply(Incremented) calls"
    );
}

// Snapshotable: capture

/// capture() returns the current counter value as a snapshot.
#[test]
fn snapshotable_capture_returns_current_value() {
    // Given
    let mut counter = Counter::default();
    counter.apply(Incremented);
    counter.apply(Incremented);

    // When
    let snapshot = counter.capture();

    // Then
    assert_eq!(snapshot, 2, "capture() must return the current value (2)");
}

/// capture() on a default counter returns 0.
#[test]
fn snapshotable_capture_default_counter_returns_zero() {
    // Given
    let counter = Counter::default();

    // When
    let snapshot = counter.capture();

    // Then
    assert_eq!(snapshot, 0, "capture() of a default Counter must return 0");
}

// Snapshotable: restore

/// restore(n) creates a Counter with value n.
#[test]
fn snapshotable_restore_sets_value() {
    // Given / When
    let restored = Counter::restore(42);

    // Then
    assert_eq!(
        restored.value, 42,
        "restore(42) must produce a Counter with value 42"
    );
}

/// restore(0) creates a Counter with value 0.
#[test]
fn snapshotable_restore_zero_produces_zero_value() {
    // Given / When
    let restored = Counter::restore(0);

    // Then
    assert_eq!(
        restored.value, 0,
        "restore(0) must produce a Counter with value 0"
    );
}

// Snapshotable: capture → restore roundtrip

/// capture() followed by restore() reproduces the original state.
#[test]
fn snapshot_roundtrip_preserves_counter_value() {
    // Given
    let mut counter = Counter::default();
    counter.apply(Incremented);
    counter.apply(Incremented);
    counter.apply(Incremented);

    // When
    let snapshot = counter.capture();
    let restored = Counter::restore(snapshot);

    // Then
    assert_eq!(
        restored.value, counter.value,
        "restored Counter must have the same value as original"
    );
}

/// capture() of default (value 0) restores to value 0.
#[test]
fn snapshot_roundtrip_for_default_counter() {
    // Given
    let counter = Counter::default();

    // When
    let snapshot = counter.capture();
    let restored = Counter::restore(snapshot);

    // Then
    assert_eq!(
        restored.value, 0,
        "restored Counter must have value 0 for default state"
    );
}
