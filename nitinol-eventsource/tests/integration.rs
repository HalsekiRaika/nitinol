//! The integration tests of `nitinol-eventsource`, compiled into one binary.
//!
//! The tests beside this one keep binaries of their own. Each of them but `ui`
//! installs a process-wide tracing subscriber, which a process holds one of;
//! `ui` drives the compiler over fixtures of its own.

#[path = "common/batch_ledger.rs"]
mod batch_ledger;
#[path = "common/recording_journal.rs"]
mod recording_journal;

#[path = "integration/aggregate.rs"]
mod aggregate;
#[path = "integration/aggregate_conflict.rs"]
mod aggregate_conflict;
#[path = "integration/aggregate_decision.rs"]
mod aggregate_decision;
#[path = "integration/aggregate_error_retryability.rs"]
mod aggregate_error_retryability;
#[path = "integration/aggregate_process.rs"]
mod aggregate_process;
#[path = "integration/aggregate_props_holds_event_store.rs"]
mod aggregate_props_holds_event_store;
#[path = "integration/aggregate_proxy_snapshot.rs"]
mod aggregate_proxy_snapshot;
#[path = "integration/aggregate_proxy_sync_entry.rs"]
mod aggregate_proxy_sync_entry;
#[path = "integration/aggregate_resolve.rs"]
mod aggregate_resolve;
#[cfg(feature = "test-helpers")]
#[path = "integration/aggregate_tell_target_trait.rs"]
mod aggregate_tell_target_trait;
#[path = "integration/aggregate_uses_the_codec_its_event_names.rs"]
mod aggregate_uses_the_codec_its_event_names;
#[path = "integration/catchup.rs"]
mod catchup;
#[path = "integration/cheap_clone_handles.rs"]
mod cheap_clone_handles;
#[path = "integration/codec.rs"]
mod codec;
#[path = "integration/codec_injection_api_removed.rs"]
mod codec_injection_api_removed;
#[path = "integration/contingent_system_carries_the_snapshot_slot.rs"]
mod contingent_system_carries_the_snapshot_slot;
#[path = "integration/delivery_mode.rs"]
mod delivery_mode;
#[path = "integration/durable_poller_config.rs"]
mod durable_poller_config;
#[path = "integration/durable_stream.rs"]
mod durable_stream;
#[path = "integration/durable_stream_receives_any_journal.rs"]
mod durable_stream_receives_any_journal;
#[path = "integration/e2e_aggregate.rs"]
mod e2e_aggregate;
#[path = "integration/e2e_offset_delivery.rs"]
mod e2e_offset_delivery;
#[path = "integration/e2e_projection.rs"]
mod e2e_projection;
#[path = "integration/e2e_replay.rs"]
mod e2e_replay;
#[path = "integration/e2e_snapshot.rs"]
mod e2e_snapshot;
#[path = "integration/event_appending.rs"]
mod event_appending;
#[path = "integration/event_type_identity.rs"]
mod event_type_identity;
#[path = "integration/event_type_matching.rs"]
mod event_type_matching;
#[path = "integration/hosted_props_spawn.rs"]
mod hosted_props_spawn;
#[cfg(feature = "test-helpers")]
#[path = "integration/mock_aggregate_proxy.rs"]
mod mock_aggregate_proxy;
#[path = "integration/projection_cursor_hold.rs"]
mod projection_cursor_hold;
#[path = "integration/projection_id.rs"]
mod projection_id;
#[path = "integration/projector.rs"]
mod projector;
#[path = "integration/projector_batch.rs"]
mod projector_batch;
#[path = "integration/projector_batch_limit.rs"]
mod projector_batch_limit;
#[path = "integration/projector_poll_interval.rs"]
mod projector_poll_interval;
#[path = "integration/projector_props_holds_event_store.rs"]
mod projector_props_holds_event_store;
#[path = "integration/projector_uses_the_codec_its_event_names.rs"]
mod projector_uses_the_codec_its_event_names;
#[path = "integration/props_take_their_stores_from_the_system.rs"]
mod props_take_their_stores_from_the_system;
#[path = "integration/proxy_error_conversion.rs"]
mod proxy_error_conversion;
#[path = "integration/query.rs"]
mod query;
#[path = "integration/replay.rs"]
mod replay;
#[path = "integration/snapshot_persistor.rs"]
mod snapshot_persistor;
#[path = "integration/snapshot_uses_the_codec_its_aggregate_names.rs"]
mod snapshot_uses_the_codec_its_aggregate_names;
#[path = "integration/system_handle_clone.rs"]
mod system_handle_clone;
#[path = "integration/system_integration.rs"]
mod system_integration;
#[path = "integration/system_persistence.rs"]
mod system_persistence;
#[path = "integration/tx_wiring.rs"]
mod tx_wiring;
#[path = "integration/typestate_spawn.rs"]
mod typestate_spawn;
