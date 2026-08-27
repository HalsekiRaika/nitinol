//! Acceptance scenario "既存利用者の互換維持": moving the contract traits into
//! `nitinol-contract` must keep `nitinol::eventsource::{Event, Aggregate,
//! Snapshotable}` denoting the *same* trait items as `nitinol::contract::{...}`,
//! not look-alike wrappers.
//!
//! Each trait is implemented once through the contract path and once through the
//! eventsource path, and every type is then checked against both bounds. A
//! forwarding wrapper trait (`trait Aggregate: contract::Aggregate` plus a
//! blanket impl) satisfies the contract-to-eventsource direction but makes the
//! direct eventsource impls below conflict, so it fails to compile here.
//!
//! Run with: `cargo test -p nitinol --features eventsource`.
#![cfg(feature = "eventsource")]

use nitinol::persistence::{EventType, Family, TypeName};

#[derive(Clone)]
struct ContractPathEvent;

impl nitinol::contract::Event for ContractPathEvent {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("identity"), TypeName::new("ContractPathEvent"));
}

#[derive(Clone)]
struct EventsourcePathEvent;

impl nitinol::eventsource::Event for EventsourcePathEvent {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("identity"),
        TypeName::new("EventsourcePathEvent"),
    );
}

#[derive(Default)]
struct ContractPathAggregate;

impl nitinol::contract::Aggregate for ContractPathAggregate {
    type Event = ContractPathEvent;

    fn apply(&mut self, _event: ContractPathEvent) {}
}

impl nitinol::contract::Snapshotable for ContractPathAggregate {
    type Snapshot = ();

    fn capture(&self) {}

    fn restore(_snapshot: ()) -> Self {
        ContractPathAggregate
    }
}

#[derive(Default)]
struct EventsourcePathAggregate;

impl nitinol::eventsource::Aggregate for EventsourcePathAggregate {
    type Event = EventsourcePathEvent;

    fn apply(&mut self, _event: EventsourcePathEvent) {}
}

impl nitinol::eventsource::Snapshotable for EventsourcePathAggregate {
    type Snapshot = ();

    fn capture(&self) {}

    fn restore(_snapshot: ()) -> Self {
        EventsourcePathAggregate
    }
}

fn requires_contract_event<E: nitinol::contract::Event>() {}
fn requires_eventsource_event<E: nitinol::eventsource::Event>() {}
fn requires_contract_aggregate<A: nitinol::contract::Aggregate>() {}
fn requires_eventsource_aggregate<A: nitinol::eventsource::Aggregate>() {}
fn requires_contract_snapshotable<A: nitinol::contract::Snapshotable>() {}
fn requires_eventsource_snapshotable<A: nitinol::eventsource::Snapshotable>() {}

/// Given a type implementing a contract trait through one public path, When it
/// is used where the other public path's trait is required, Then it satisfies
/// the bound — the two paths name one trait item.
#[test]
fn contract_and_eventsource_paths_name_the_same_traits() {
    requires_contract_event::<ContractPathEvent>();
    requires_eventsource_event::<ContractPathEvent>();
    requires_contract_event::<EventsourcePathEvent>();
    requires_eventsource_event::<EventsourcePathEvent>();

    requires_contract_aggregate::<ContractPathAggregate>();
    requires_eventsource_aggregate::<ContractPathAggregate>();
    requires_contract_aggregate::<EventsourcePathAggregate>();
    requires_eventsource_aggregate::<EventsourcePathAggregate>();

    requires_contract_snapshotable::<ContractPathAggregate>();
    requires_eventsource_snapshotable::<ContractPathAggregate>();
    requires_contract_snapshotable::<EventsourcePathAggregate>();
    requires_eventsource_snapshotable::<EventsourcePathAggregate>();
}

/// Given an aggregate implemented through the contract path only, When it is
/// passed to the eventsource process builder, Then the existing runtime API
/// accepts it unchanged.
///
/// `AggregateProps` states its bound in terms of `nitinol-eventsource`'s own
/// internal path, so this fails — where the bound checks above still pass — if
/// the crate re-exports the moved trait at its root while some internal module
/// keeps binding a leftover local definition.
#[test]
fn eventsource_process_builder_accepts_a_contract_path_aggregate() {
    let _ = std::marker::PhantomData::<nitinol::eventsource::AggregateProps<ContractPathAggregate>>;
}
