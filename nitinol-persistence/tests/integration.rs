//! The integration tests of `nitinol-persistence`, compiled into one binary.
//!
//! `event_registry_link_collision` keeps a binary of its own: a binary links one
//! registry, and the entries that test links collide on purpose.

#[path = "integration/arc_handle_api_removed.rs"]
mod arc_handle_api_removed;
#[path = "integration/checkpoint_api_removed.rs"]
mod checkpoint_api_removed;
#[path = "integration/event_registry.rs"]
mod event_registry;
#[path = "integration/event_registry_constructors_are_not_public.rs"]
mod event_registry_constructors_are_not_public;
#[path = "integration/event_registry_linked.rs"]
mod event_registry_linked;
#[path = "integration/event_store.rs"]
mod event_store;
#[path = "integration/event_store_load_query_conjunction.rs"]
mod event_store_load_query_conjunction;
#[path = "integration/event_store_occ.rs"]
mod event_store_occ;
#[path = "integration/event_store_prefix_search.rs"]
mod event_store_prefix_search;
#[path = "integration/event_store_str_key.rs"]
mod event_store_str_key;
#[path = "integration/event_type.rs"]
mod event_type;
#[path = "integration/journal_clone_shares_backend_state.rs"]
mod journal_clone_shares_backend_state;
#[path = "integration/journal_names_no_slot_beyond_itself.rs"]
mod journal_names_no_slot_beyond_itself;
#[path = "integration/materialized_path.rs"]
mod materialized_path;
#[path = "integration/offset_scope.rs"]
mod offset_scope;
#[path = "integration/offset_store.rs"]
mod offset_store;
#[path = "integration/persistence_provider.rs"]
mod persistence_provider;
#[path = "integration/reserved_namespace.rs"]
mod reserved_namespace;
#[path = "integration/snapshot_store.rs"]
mod snapshot_store;
