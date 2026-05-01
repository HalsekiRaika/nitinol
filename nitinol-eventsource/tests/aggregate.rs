use nitinol_eventsource::{
    Aggregate, Context, Event, Snapshotable, SnapshotCaptureError, SnapshotRestoreError,
};
use nitinol_persistence::{AggregateId, EventType};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Incremented;

impl Event for Incremented {
    const EVENT_TYPE: EventType = EventType::from_str("Incremented");
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

// Snapshot encodes value as 8 big-endian bytes.
impl Snapshotable for Counter {
    fn restore(payload: &[u8]) -> Result<Self, SnapshotRestoreError> {
        let arr: [u8; 8] = payload
            .try_into()
            .map_err(|e: std::array::TryFromSliceError| SnapshotRestoreError::Decode(Box::new(e)))?;
        Ok(Counter {
            value: u64::from_be_bytes(arr),
        })
    }

    fn capture(&self) -> Result<bytes::Bytes, SnapshotCaptureError> {
        Ok(bytes::Bytes::from(self.value.to_be_bytes().to_vec()))
    }
}

// ---------------------------------------------------------------------------
// Event: EVENT_TYPE constant
// ---------------------------------------------------------------------------

/// EVENT_TYPE constant carries the correct string value
#[test]
fn event_type_constant_has_correct_string() {
    // Given / When / Then
    assert_eq!(
        Incremented::EVENT_TYPE.as_str(),
        "Incremented",
        "EVENT_TYPE must be the static string 'Incremented'"
    );
}

// ---------------------------------------------------------------------------
// Aggregate: Default initial state
// ---------------------------------------------------------------------------

/// Counter::default() starts with value 0
#[test]
fn default_counter_has_value_zero() {
    // Given / When
    let counter = Counter::default();

    // Then
    assert_eq!(counter.value, 0, "initial Counter value must be 0");
}

// ---------------------------------------------------------------------------
// Aggregate: apply updates state
// ---------------------------------------------------------------------------

/// apply(Incremented) increments Counter.value by 1
#[test]
fn apply_incremented_increments_value() {
    // Given
    let mut counter = Counter::default();

    // When
    counter.apply(Incremented);

    // Then
    assert_eq!(counter.value, 1, "value must be 1 after one apply(Incremented)");
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

// ---------------------------------------------------------------------------
// Context: constructor and accessors
// ---------------------------------------------------------------------------

/// Context::new stores the given aggregate_id and sequence
#[test]
fn context_new_stores_aggregate_id_and_sequence() {
    // Given / When
    let ctx = Context::new(AggregateId::new("agg-001"), 42);

    // Then
    assert_eq!(
        ctx.aggregate_id().as_str(),
        "agg-001",
        "aggregate_id must match the value passed to new()"
    );
    assert_eq!(ctx.sequence(), 42, "sequence must match the value passed to new()");
}

/// Context::sequence returns 0 when constructed with sequence 0
#[test]
fn context_sequence_zero_is_valid() {
    // Given / When
    let ctx = Context::new(AggregateId::new("agg-zero"), 0);

    // Then
    assert_eq!(ctx.sequence(), 0, "sequence 0 must be a valid initial sequence");
}

// ---------------------------------------------------------------------------
// Snapshotable: capture → restore roundtrip
// ---------------------------------------------------------------------------

/// capture() followed by restore() reproduces the original state
#[test]
fn snapshot_roundtrip_preserves_counter_value() {
    // Given
    let mut counter = Counter::default();
    counter.apply(Incremented);
    counter.apply(Incremented);
    counter.apply(Incremented);

    // When
    let bytes = counter.capture().expect("capture must succeed");
    let restored = Counter::restore(&bytes).expect("restore must succeed");

    // Then
    assert_eq!(
        restored.value, counter.value,
        "restored Counter must have the same value as original"
    );
}

/// capture() of default (value 0) restores to value 0
#[test]
fn snapshot_roundtrip_for_default_counter() {
    // Given
    let counter = Counter::default();

    // When
    let bytes = counter.capture().expect("capture must succeed");
    let restored = Counter::restore(&bytes).expect("restore must succeed");

    // Then
    assert_eq!(restored.value, 0, "restored Counter must have value 0 for default state");
}
