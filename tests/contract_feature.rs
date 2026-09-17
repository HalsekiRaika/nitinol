//! Acceptance scenario "tokio 非依存の契約層 / tokio-free 構成での利用":
//! with `nitinol = { features = ["contract"] }` alone, `#[derive(Event)]`,
//! `Aggregate`, `Snapshotable`, `Decider` and `Query` must be usable and
//! behave as specified.
//!
//! Everything here is reached through `nitinol::contract` and
//! `nitinol::persistence` only. Referring to `nitinol::eventsource` or
//! `nitinol::runtime` would make the file compile only in a Tokio-bearing
//! configuration and would silently void the scenario.
//!
//! Run with: `cargo test -p nitinol --no-default-features --features contract`.
#![cfg(feature = "contract")]

use bytes::Bytes;
use nitinol::contract::{
    Accepting, Aggregate, Codec, Decider, Decision, Event, Query, Snapshotable,
};
use nitinol::persistence::{EventType, Family, TypeName, Variant};

#[derive(Clone, Event)]
#[event(family = "shop.orders", codec = PlacedCodec)]
struct Placed;

struct PlacedCodec;

impl Codec<Placed> for PlacedCodec {
    type Error = std::convert::Infallible;

    fn encode(_event: &Placed) -> Result<Bytes, Self::Error> {
        Ok(Bytes::new())
    }

    fn decode(_payload: &[u8]) -> Result<Placed, Self::Error> {
        Ok(Placed)
    }
}

#[derive(Clone, Event)]
#[event(family = "shop.counter", codec = CounterEventCodec)]
enum CounterEvent {
    Incremented,
    Decremented,
}

struct CounterEventCodec;

impl Codec<CounterEvent> for CounterEventCodec {
    type Error = std::convert::Infallible;

    fn encode(_event: &CounterEvent) -> Result<Bytes, Self::Error> {
        Ok(Bytes::new())
    }

    fn decode(_payload: &[u8]) -> Result<CounterEvent, Self::Error> {
        Ok(CounterEvent::Incremented)
    }
}

/// The snapshot value is its own format, named by `Snapshotable::Codec`.
struct CounterSnapshotCodec;

impl Codec<i64> for CounterSnapshotCodec {
    type Error = std::convert::Infallible;

    fn encode(value: &i64) -> Result<Bytes, Self::Error> {
        Ok(Bytes::from(value.to_be_bytes().to_vec()))
    }

    fn decode(payload: &[u8]) -> Result<i64, Self::Error> {
        let mut buf = [0u8; 8];
        let taken = payload.len().min(8);
        buf[..taken].copy_from_slice(&payload[..taken]);
        Ok(i64::from_be_bytes(buf))
    }
}

#[derive(Default)]
struct Counter {
    value: i64,
}

impl Aggregate for Counter {
    type Event = CounterEvent;

    fn apply(&mut self, event: CounterEvent) {
        match event {
            CounterEvent::Incremented => self.value += 1,
            CounterEvent::Decremented => self.value -= 1,
        }
    }
}

impl Snapshotable for Counter {
    type Snapshot = i64;
    type Codec = CounterSnapshotCodec;

    fn capture(&self) -> i64 {
        self.value
    }

    fn restore(snapshot: i64) -> Self {
        Counter { value: snapshot }
    }
}

struct Increment;
struct AtCeiling;

impl Decider<Increment> for Counter {
    type Output = i64;
    type Rejection = AtCeiling;

    fn decide(&self, _: Increment) -> Decision<CounterEvent, i64, AtCeiling> {
        // Named explicitly (rather than chained) to prove `Accepting` itself
        // resolves through `nitinol::contract`, not just `Decider`/`Decision`.
        let accepting: Accepting<CounterEvent, i64, AtCeiling> =
            Decision::persist(vec![CounterEvent::Incremented]);
        accepting.output(self.value + 1)
    }
}

struct CurrentValue;

impl Query<CurrentValue> for Counter {
    type Response = i64;
    type Error = std::convert::Infallible;

    fn query(&self, _: CurrentValue) -> Result<i64, std::convert::Infallible> {
        Ok(self.value)
    }
}

/// Given a struct deriving `Event` in a `contract`-only build, When its
/// `EVENT_TYPE` is read and `variant()` is called, Then the derive resolves the
/// trait through the contract path and the value-level identity falls back to
/// the trait's default (type-level, variant `None`).
#[test]
fn derived_struct_event_is_usable_through_the_contract_facade() {
    assert_eq!(
        Placed::EVENT_TYPE,
        EventType::new(Family::new("shop.orders"), TypeName::new("Placed")),
    );
    assert_eq!(Placed.variant(), Placed::EVENT_TYPE);
    assert_eq!(Placed.variant().variant(), None);
}

/// Given an enum deriving `Event` in a `contract`-only build, When `variant()`
/// is called on an arm, Then the generated match names that arm — proving the
/// generated `::nitinol::persistence::{EventType, Family, TypeName, Variant}`
/// references resolve without the `eventsource` feature.
#[test]
fn derived_enum_event_names_the_active_arm() {
    assert_eq!(
        CounterEvent::Incremented.variant(),
        EventType::with_variant(
            Family::new("shop.counter"),
            TypeName::new("CounterEvent"),
            Variant::new("Incremented"),
        ),
    );
    assert_eq!(
        CounterEvent::Decremented.variant().variant(),
        Some(Variant::new("Decremented")),
    );
    assert_eq!(CounterEvent::EVENT_TYPE.variant(), None);
}

/// Given an aggregate implemented against `nitinol::contract::Aggregate`, When
/// events are applied, Then state advances synchronously — no async runtime is
/// constructed anywhere in this test, which is the reason the contract crate
/// exists.
#[test]
fn aggregate_applies_events_without_an_async_runtime() {
    let mut counter = Counter::default();

    counter.apply(CounterEvent::Incremented);
    counter.apply(CounterEvent::Incremented);
    counter.apply(CounterEvent::Decremented);

    assert_eq!(counter.value, 1);
}

/// Given a `Snapshotable` aggregate, When a snapshot is captured and restored,
/// Then the restored aggregate resumes from the captured state and keeps
/// applying events from there.
#[test]
fn snapshot_capture_and_restore_round_trips() {
    let mut counter = Counter::default();
    counter.apply(CounterEvent::Incremented);
    counter.apply(CounterEvent::Incremented);

    let snapshot = counter.capture();
    assert_eq!(snapshot, 2);

    let mut restored = Counter::restore(snapshot);
    assert_eq!(restored.value, 2);

    restored.apply(CounterEvent::Decremented);
    assert_eq!(restored.value, 1);
}

/// Given a `Decider` and a `Query` implemented against `nitinol::contract`,
/// When a command is decided and the state is queried, Then both resolve and
/// answer synchronously — proving `Decider`, `Decision`, `Accepting` and
/// `Query` are reachable through the `contract` facade, not just through
/// `nitinol::eventsource`.
#[test]
fn decider_and_query_are_usable_through_the_contract_facade() {
    let counter = Counter::default();

    let decision = counter.decide(Increment);
    assert!(matches!(decision, Decision::Accept { output: 1, .. }));

    assert_eq!(counter.query(CurrentValue), Ok(0));
}
