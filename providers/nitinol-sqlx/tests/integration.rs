//! The integration tests of `nitinol-sqlx`, compiled into one binary.

#[path = "common/fixture.rs"]
mod fixture;
#[path = "common/harness.rs"]
mod harness;

#[path = "integration/exactly_once_projection.rs"]
mod exactly_once_projection;
#[path = "integration/journal_event_type_index.rs"]
mod journal_event_type_index;
#[path = "integration/load_query.rs"]
mod load_query;
#[path = "integration/load_query_conjunction.rs"]
mod load_query_conjunction;
#[path = "integration/occ_contract.rs"]
mod occ_contract;
#[path = "integration/offset_conformance.rs"]
mod offset_conformance;
#[path = "integration/offset_store.rs"]
mod offset_store;
#[path = "integration/offset_table_shape.rs"]
mod offset_table_shape;
#[path = "integration/provider_conformance.rs"]
mod provider_conformance;
#[path = "integration/round_trip.rs"]
mod round_trip;
#[path = "integration/snapshot_store.rs"]
mod snapshot_store;
#[path = "integration/snapshot_table_shape.rs"]
mod snapshot_table_shape;
#[path = "integration/snapshot_wiring.rs"]
mod snapshot_wiring;
#[path = "integration/store_conformance.rs"]
mod store_conformance;
