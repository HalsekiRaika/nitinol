//! Acceptance scenario "existing users keep compatibility": moving the
//! contract traits into
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
//! The identity types follow the same rule in the other direction. They are
//! defined in `nitinol-contract`, which is where a domain promises the machinery
//! its event identities, and `nitinol::persistence` re-exports them because an
//! event type is part of the shape of a stored row. Both paths must name the one
//! type: a value built through either is accepted wherever the other is
//! required, and the registry the derive places its entries in holds the
//! contract's entry. A look-alike copy left in persistence fails to compile
//! here.
//!
//! Run with: `cargo test -p nitinol --features eventsource`.
#![cfg(feature = "eventsource")]

use bytes::Bytes;
use nitinol::contract::{EventType, Family, TypeName};

/// One codec for every fixture value here: the file is about trait identity
/// across the two public paths, so a format is only needed to satisfy the
/// association at all.
struct IdentityCodec;

impl<T: Default> nitinol::contract::Codec<T> for IdentityCodec {
    type Error = std::convert::Infallible;

    fn encode(_value: &T) -> Result<Bytes, Self::Error> {
        Ok(Bytes::new())
    }

    fn decode(_payload: &[u8]) -> Result<T, Self::Error> {
        Ok(T::default())
    }
}

#[derive(Clone, Default)]
struct ContractPathEvent;

impl nitinol::contract::Event for ContractPathEvent {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("identity"), TypeName::new("ContractPathEvent"));

    type Codec = IdentityCodec;
}

#[derive(Clone, Default)]
struct EventsourcePathEvent;

impl nitinol::eventsource::Event for EventsourcePathEvent {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("identity"),
        TypeName::new("EventsourcePathEvent"),
    );

    type Codec = IdentityCodec;
}

#[derive(Default)]
struct ContractPathAggregate;

impl nitinol::contract::Aggregate for ContractPathAggregate {
    type Event = ContractPathEvent;
    // Named through the *other* path on purpose: the recovery marker is subject
    // to the same identity claim as the traits, so a contract-path aggregate
    // satisfying `Recovery<Self>` with the eventsource-path `Latest` is what
    // says the two paths denote one marker.
    type Recover = nitinol::eventsource::Latest;

    fn apply(&mut self, _event: ContractPathEvent) {}
}

impl nitinol::contract::Snapshotable for ContractPathAggregate {
    type Snapshot = ();
    type Codec = IdentityCodec;

    fn capture(&self) {}

    fn recover(_snapshot: ()) -> Self {
        ContractPathAggregate
    }
}

#[derive(Default)]
struct EventsourcePathAggregate;

impl nitinol::eventsource::Aggregate for EventsourcePathAggregate {
    type Event = EventsourcePathEvent;
    type Recover = nitinol::contract::Latest;

    fn apply(&mut self, _event: EventsourcePathEvent) {}
}

impl nitinol::eventsource::Snapshotable for EventsourcePathAggregate {
    type Snapshot = ();
    type Codec = IdentityCodec;

    fn capture(&self) {}

    fn recover(_snapshot: ()) -> Self {
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

// The identity types: one definition, named through two paths.

fn into_persistence_event_type(
    value: nitinol::persistence::EventType,
) -> nitinol::persistence::EventType {
    value
}
fn into_contract_event_type(value: nitinol::contract::EventType) -> nitinol::contract::EventType {
    value
}
fn into_persistence_family(value: nitinol::persistence::Family) -> nitinol::persistence::Family {
    value
}
fn into_contract_family(value: nitinol::contract::Family) -> nitinol::contract::Family {
    value
}
fn into_persistence_type_name(
    value: nitinol::persistence::TypeName,
) -> nitinol::persistence::TypeName {
    value
}
fn into_contract_type_name(value: nitinol::contract::TypeName) -> nitinol::contract::TypeName {
    value
}
fn into_persistence_path(
    value: nitinol::persistence::MaterializedPath,
) -> nitinol::persistence::MaterializedPath {
    value
}
fn into_contract_path(
    value: nitinol::contract::MaterializedPath,
) -> nitinol::contract::MaterializedPath {
    value
}
fn into_persistence_parsed(
    value: nitinol::persistence::ParsedEventType,
) -> nitinol::persistence::ParsedEventType {
    value
}
fn into_contract_parsed(
    value: nitinol::contract::ParsedEventType,
) -> nitinol::contract::ParsedEventType {
    value
}
fn into_persistence_parse_error(
    value: nitinol::persistence::MaterializedPathParseError,
) -> nitinol::persistence::MaterializedPathParseError {
    value
}
fn into_contract_parse_error(
    value: nitinol::contract::MaterializedPathParseError,
) -> nitinol::contract::MaterializedPathParseError {
    value
}
fn persistence_entry_declared_by(entry: &nitinol::persistence::EventRegistryEntry) -> &'static str {
    entry.declared_by()
}
fn contract_entry_declared_by(entry: &nitinol::contract::EventRegistryEntry) -> &'static str {
    entry.declared_by()
}

/// Given an event type built through the contract path, When it is passed where
/// the persistence path's type is required and back again, Then it is accepted
/// both ways and comes back unchanged — and the same holds for its components.
#[test]
fn a_contract_path_event_type_is_the_persistence_path_event_type() {
    let declared: nitinol::contract::EventType = nitinol::contract::EventType::new(
        nitinol::contract::Family::new("identity"),
        nitinol::contract::TypeName::new("Declared"),
    );

    let through_persistence = into_persistence_event_type(declared);
    let back: nitinol::contract::EventType = into_contract_event_type(through_persistence);
    assert_eq!(back, declared);

    let family = into_contract_family(into_persistence_family(declared.family()));
    assert_eq!(family, declared.family());
    let type_name = into_contract_type_name(into_persistence_type_name(declared.type_name()));
    assert_eq!(type_name, declared.type_name());
}

/// Given an event type built through the persistence path, When it is passed
/// where the contract path's type is required and back again, Then it is
/// accepted both ways — the direction a stored row's event type travels when a
/// domain reads it.
#[test]
fn a_persistence_path_event_type_is_the_contract_path_event_type() {
    let stored: nitinol::persistence::EventType = nitinol::persistence::EventType::new(
        nitinol::persistence::Family::new("identity"),
        nitinol::persistence::TypeName::new("Stored"),
    );

    let through_contract = into_contract_event_type(stored);
    let back: nitinol::persistence::EventType = into_persistence_event_type(through_contract);
    assert_eq!(back, stored);

    let family = into_persistence_family(into_contract_family(stored.family()));
    assert_eq!(family, stored.family());
    let type_name = into_persistence_type_name(into_contract_type_name(stored.type_name()));
    assert_eq!(type_name, stored.type_name());
}

/// Given the owned projections of an identity — a path, its decomposition and
/// a parse refusal — When each is passed through both paths, Then it is
/// accepted both ways.
#[test]
fn the_path_types_are_one_type_through_either_path() {
    let path: nitinol::contract::MaterializedPath = "identity.Declared"
        .parse()
        .expect("a canonical path parses");
    let path = into_contract_path(into_persistence_path(path));
    assert_eq!(path.to_string(), "identity.Declared");

    let parsed = into_contract_parsed(into_persistence_parsed(
        nitinol::contract::ParsedEventType::from_path(path),
    ));
    let parsed = into_persistence_parsed(into_contract_parsed(parsed));
    assert_eq!(parsed.to_string(), "identity.Declared");

    let refused = ""
        .parse::<nitinol::persistence::MaterializedPath>()
        .expect_err("an empty string is not a path");
    let refused = into_persistence_parse_error(into_contract_parse_error(refused));
    assert!(matches!(
        refused,
        nitinol::contract::MaterializedPathParseError::Empty
    ));
}

const CONTRACT_DECLARED: EventType =
    EventType::new(Family::new("identity"), TypeName::new("ContractEntry"));

/// Placed the way `#[derive(Event)]` places its entry: built through the
/// contract path, collected by the persistence registry.
#[::nitinol::__private::linkme::distributed_slice(::nitinol::persistence::EVENT_REGISTRY)]
#[linkme(crate = ::nitinol::__private::linkme)]
static CONTRACT_ENTRY: nitinol::contract::EventRegistryEntry =
    nitinol::contract::EventRegistryEntry::new(
        "contract_reexport_identity::ContractEntry",
        &[CONTRACT_DECLARED],
    );

const PERSISTENCE_DECLARED: nitinol::persistence::EventType = nitinol::persistence::EventType::new(
    nitinol::persistence::Family::new("identity"),
    nitinol::persistence::TypeName::new("PersistenceEntry"),
);
static PERSISTENCE_ENTRY: nitinol::persistence::EventRegistryEntry =
    nitinol::persistence::EventRegistryEntry::new(
        "contract_reexport_identity::PersistenceEntry",
        &[PERSISTENCE_DECLARED],
    );

/// Given registry entries built through each path, When each is handed to a
/// function that reads the other path's entry, Then it is accepted — the entry
/// a declaration tells the machinery its path with is one type.
#[test]
fn a_registry_entry_is_one_type_through_either_path() {
    assert_eq!(
        persistence_entry_declared_by(&CONTRACT_ENTRY),
        "contract_reexport_identity::ContractEntry"
    );
    assert_eq!(
        contract_entry_declared_by(&PERSISTENCE_ENTRY),
        "contract_reexport_identity::PersistenceEntry"
    );
}

/// Given an entry built through the contract path and placed in the registry
/// the persistence path owns, When the registry's elements are read as the
/// contract's `EventRegistryEntry`, Then the placed entry is among them with
/// the event types it claimed — the registry collects the contract's entry, not
/// a persistence copy of it.
#[test]
fn the_registry_collects_the_contract_entry() {
    let collected: &[nitinol::contract::EventRegistryEntry] = &nitinol::persistence::EVENT_REGISTRY;

    let placed = collected
        .iter()
        .find(|entry| entry.declared_by() == "contract_reexport_identity::ContractEntry")
        .expect("the entry placed through the contract path is collected");
    assert_eq!(placed.event_types(), &[CONTRACT_DECLARED]);
}
