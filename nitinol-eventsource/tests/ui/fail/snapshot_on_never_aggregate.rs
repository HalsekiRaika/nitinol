// `Counter` is `Snapshotable`, so the only bound it misses is `Recover =
// Latest`: that is the one reason `snapshot()` may be unavailable here.

use bytes::Bytes;
use nitinol_eventsource::codec::Codec;
use nitinol_eventsource::{Aggregate, AggregateProxy, Event, Never, Snapshotable};
use nitinol_persistence::{EventType, Family, TypeName};

#[derive(Clone)]
struct Incremented;

struct UnitCodec;

impl Codec<Incremented> for UnitCodec {
    type Error = std::convert::Infallible;

    fn encode(_event: &Incremented) -> Result<Bytes, Self::Error> {
        Ok(Bytes::new())
    }

    fn decode(_payload: &[u8]) -> Result<Incremented, Self::Error> {
        Ok(Incremented)
    }
}

impl Codec<u64> for UnitCodec {
    type Error = std::convert::Infallible;

    fn encode(value: &u64) -> Result<Bytes, Self::Error> {
        Ok(Bytes::from(value.to_be_bytes().to_vec()))
    }

    fn decode(_payload: &[u8]) -> Result<u64, Self::Error> {
        Ok(0)
    }
}

impl Event for Incremented {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("ui.snapshot"), TypeName::new("Incremented"));
    type Codec = UnitCodec;
}

#[derive(Default)]
struct Counter {
    value: u64,
}

impl Aggregate for Counter {
    type Event = Incremented;
    type Recover = Never;

    fn apply(&mut self, _event: Incremented) {
        self.value += 1;
    }
}

impl Snapshotable for Counter {
    type Snapshot = u64;
    type Codec = UnitCodec;

    fn capture(&self) -> u64 {
        self.value
    }

    fn recover(snapshot: u64) -> Self {
        Self { value: snapshot }
    }
}

async fn save(proxy: AggregateProxy<Counter>) {
    let _ = proxy.snapshot().await;
}

fn main() {
    let _ = save;
}
