//! Acceptance scenario "tokio 非依存の契約層 / tokio-free 構成での利用":
//! with `nitinol = { features = ["contract"] }` alone, `#[derive(Event)]`,
//! `Aggregate` and `Snapshotable` must be usable and behave as specified.
//!
//! Everything here is reached through `nitinol::contract` and
//! `nitinol::persistence` only. Referring to `nitinol::eventsource` or
//! `nitinol::runtime` would make the file compile only in a Tokio-bearing
//! configuration and would silently void the scenario.
//!
//! Run with: `cargo test -p nitinol --no-default-features --features contract`.
#![cfg(feature = "contract")]

use nitinol::contract::{Aggregate, Event, Snapshotable};
use nitinol::persistence::{EventType, Family, TypeName, Variant};

#[derive(Clone, Event)]
#[event(family = "shop.orders")]
struct Placed;

#[derive(Clone, Event)]
#[event(family = "shop.counter")]
enum CounterEvent {
    Incremented,
    Decremented,
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

    fn capture(&self) -> i64 {
        self.value
    }

    fn restore(snapshot: i64) -> Self {
        Counter { value: snapshot }
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
