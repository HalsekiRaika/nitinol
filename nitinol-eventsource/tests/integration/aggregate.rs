use bytes::Bytes;
use nitinol_eventsource::{codec::Codec, Aggregate, Event, Latest, Snapshotable};
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
    type Recover = Latest;

    fn apply(&mut self, _event: Incremented) {
        self.value += 1;
    }
}

/// The codec `Counter` names for its snapshot. As with the event codec, nothing
/// here persists, so the format only has to exist.
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
// writes that value down and defines the pure capture / recover functions; the
// encoding itself is never reached from this file.
impl Snapshotable for Counter {
    type Snapshot = u64;

    type Codec = CounterSnapshotCodec;

    fn capture(&self) -> u64 {
        self.value
    }

    fn recover(snapshot: u64) -> Self {
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

// Aggregate + Snapshotable: implementability

/// A plain type outside this crate can implement [`Aggregate`] and
/// [`Snapshotable`] together: `Event`/`Snapshot` and their codecs line up, the
/// `Default + Send + Sync + 'static` bounds are met, and `apply` / `capture` /
/// `recover` are callable at the signatures the traits declare.
///
/// Not a `#[test]` — the contract is that this compiles.  What the fixture's
/// own arithmetic computes is the fixture's business, so nothing here asserts
/// on values.
#[allow(dead_code)]
fn _assert_counter_implements_aggregate_and_snapshotable() {
    let mut counter = Counter::default();
    counter.apply(Incremented);

    let snapshot: <Counter as Snapshotable>::Snapshot = counter.capture();
    let _: Counter = Counter::recover(snapshot);
}
