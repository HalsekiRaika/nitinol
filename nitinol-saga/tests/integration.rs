//! The integration tests of `nitinol-saga`, compiled into one binary.
//!
//! A test that installs a process-wide tracing subscriber keeps a binary of its
//! own beside this one: a process holds one global subscriber, and such a test
//! reads every record it captures as its own. Those are the tracing tests and
//! the deadline-index tests that read the index's records.

#[path = "common/helpers.rs"]
mod common;
#[path = "common/deadline_index.rs"]
mod deadline_index;
#[path = "common/delivery.rs"]
mod delivery;
#[path = "common/manager_offsets.rs"]
mod manager_offsets;

#[path = "integration/cheap_clone_handles.rs"]
mod cheap_clone_handles;
#[path = "integration/e2e_saga.rs"]
mod e2e_saga;
#[path = "integration/effect_construction.rs"]
mod effect_construction;
#[path = "integration/effect_flatten.rs"]
mod effect_flatten;
#[path = "integration/effect_monoid.rs"]
mod effect_monoid;
#[path = "integration/persistence_delivery_without_projector.rs"]
mod persistence_delivery_without_projector;
#[path = "integration/persistence_projection_rebuild_leaves_delivery_progress.rs"]
mod persistence_projection_rebuild_leaves_delivery_progress;
#[path = "integration/saga_all_sources_must_be_bound.rs"]
mod saga_all_sources_must_be_bound;
#[path = "integration/saga_apply_is_optional.rs"]
mod saga_apply_is_optional;
#[path = "integration/saga_codec_injection_api_removed.rs"]
mod saga_codec_injection_api_removed;
#[path = "integration/saga_combine_persist_single_append.rs"]
mod saga_combine_persist_single_append;
#[path = "integration/saga_components_system.rs"]
mod saga_components_system;
#[path = "integration/saga_components_system_reaches_latest_aggregates.rs"]
mod saga_components_system_reaches_latest_aggregates;
#[path = "integration/saga_consumed_fact_declaration.rs"]
mod saga_consumed_fact_declaration;
#[path = "integration/saga_consumed_position.rs"]
mod saga_consumed_position;
#[path = "integration/saga_consumed_position_per_source.rs"]
mod saga_consumed_position_per_source;
#[path = "integration/saga_context_extension.rs"]
mod saga_context_extension;
#[path = "integration/saga_context_upstream_population.rs"]
mod saga_context_upstream_population;
#[path = "integration/saga_correlate_single_instance_routing.rs"]
mod saga_correlate_single_instance_routing;
#[path = "integration/saga_deadline_cancel.rs"]
mod saga_deadline_cancel;
#[path = "integration/saga_deadline_codec.rs"]
mod saga_deadline_codec;
#[path = "integration/saga_deadline_fired_after_decision.rs"]
mod saga_deadline_fired_after_decision;
#[path = "integration/saga_deadline_index_activates_without_upstream.rs"]
mod saga_deadline_index_activates_without_upstream;
#[path = "integration/saga_deadline_index_after_restart.rs"]
mod saga_deadline_index_after_restart;
#[path = "integration/saga_deadline_index_on_another_system.rs"]
mod saga_deadline_index_on_another_system;
#[path = "integration/saga_deadline_index_requires_declaration.rs"]
mod saga_deadline_index_requires_declaration;
#[path = "integration/saga_deadline_index_subscription_query.rs"]
mod saga_deadline_index_subscription_query;
#[path = "integration/saga_deadline_pending_view.rs"]
mod saga_deadline_pending_view;
#[path = "integration/saga_deadline_requires_component.rs"]
mod saga_deadline_requires_component;
#[path = "integration/saga_deadline_scheduler.rs"]
mod saga_deadline_scheduler;
#[path = "integration/saga_deadline_surface.rs"]
mod saga_deadline_surface;
#[path = "integration/saga_decode_failure_route_api_removed.rs"]
mod saga_decode_failure_route_api_removed;
#[path = "integration/saga_delivery_already_created_is_not_success.rs"]
mod saga_delivery_already_created_is_not_success;
#[path = "integration/saga_delivery_cursor_never_passes_undelivered.rs"]
mod saga_delivery_cursor_never_passes_undelivered;
#[path = "integration/saga_delivery_id_is_configurable.rs"]
mod saga_delivery_id_is_configurable;
#[path = "integration/saga_delivery_mapping_is_pure.rs"]
mod saga_delivery_mapping_is_pure;
#[path = "integration/saga_delivery_offset_advance_escape.rs"]
mod saga_delivery_offset_advance_escape;
#[path = "integration/saga_delivery_offset_is_per_partition.rs"]
mod saga_delivery_offset_is_per_partition;
#[path = "integration/saga_delivery_offset_save_failure.rs"]
mod saga_delivery_offset_save_failure;
#[path = "integration/saga_delivery_offset_scope.rs"]
mod saga_delivery_offset_scope;
#[path = "integration/saga_delivery_partition_function.rs"]
mod saga_delivery_partition_function;
#[path = "integration/saga_delivery_poll_interval.rs"]
mod saga_delivery_poll_interval;
#[path = "integration/saga_delivery_preserves_instance_order.rs"]
mod saga_delivery_preserves_instance_order;
#[path = "integration/saga_delivery_reaches_a_latest_target.rs"]
mod saga_delivery_reaches_a_latest_target;
#[path = "integration/saga_delivery_reaches_target.rs"]
mod saga_delivery_reaches_target;
#[path = "integration/saga_delivery_rejection_is_observed.rs"]
mod saga_delivery_rejection_is_observed;
#[path = "integration/saga_delivery_restart_rereads_offset.rs"]
mod saga_delivery_restart_rereads_offset;
#[path = "integration/saga_delivery_resumes_from_offset.rs"]
mod saga_delivery_resumes_from_offset;
#[path = "integration/saga_delivery_scope_api_removed.rs"]
mod saga_delivery_scope_api_removed;
#[path = "integration/saga_delivery_stall_is_partition_local.rs"]
mod saga_delivery_stall_is_partition_local;
#[path = "integration/saga_delivery_subscription_query.rs"]
mod saga_delivery_subscription_query;
#[path = "integration/saga_empty_persist_is_noop.rs"]
mod saga_empty_persist_is_noop;
#[path = "integration/saga_end_ended_append_failure_no_stop.rs"]
mod saga_end_ended_append_failure_no_stop;
#[path = "integration/saga_end_variant.rs"]
mod saga_end_variant;
#[path = "integration/saga_enum_event_persist_identity.rs"]
mod saga_enum_event_persist_identity;
#[path = "integration/saga_events_are_link_collected.rs"]
mod saga_events_are_link_collected;
#[path = "integration/saga_first_read_is_immediate.rs"]
mod saga_first_read_is_immediate;
#[path = "integration/saga_handle_error_settles_with_position_only.rs"]
mod saga_handle_error_settles_with_position_only;
#[path = "integration/saga_host_delivery_configuration.rs"]
mod saga_host_delivery_configuration;
#[path = "integration/saga_host_entry_point.rs"]
mod saga_host_entry_point;
#[path = "integration/saga_host_owns_scheduler.rs"]
mod saga_host_owns_scheduler;
#[path = "integration/saga_host_starts_the_delivery_host.rs"]
mod saga_host_starts_the_delivery_host;
#[path = "integration/saga_hosted_props_spawn.rs"]
mod saga_hosted_props_spawn;
#[path = "integration/saga_id_newtype.rs"]
mod saga_id_newtype;
#[path = "integration/saga_id_reserved_namespace.rs"]
mod saga_id_reserved_namespace;
#[path = "integration/saga_lifecycle_ended_declaration.rs"]
mod saga_lifecycle_ended_declaration;
#[path = "integration/saga_lifecycle_trait_defaults.rs"]
mod saga_lifecycle_trait_defaults;
#[path = "integration/saga_manager_cursor_semantics.rs"]
mod saga_manager_cursor_semantics;
#[path = "integration/saga_manager_degenerate_scheduler.rs"]
mod saga_manager_degenerate_scheduler;
#[path = "integration/saga_manager_degenerate_single_instance.rs"]
mod saga_manager_degenerate_single_instance;
#[path = "integration/saga_manager_floor_is_kept.rs"]
mod saga_manager_floor_is_kept;
#[path = "integration/saga_manager_fronted_end_cancels_live_timer.rs"]
mod saga_manager_fronted_end_cancels_live_timer;
#[path = "integration/saga_manager_lazy_spawn.rs"]
mod saga_manager_lazy_spawn;
#[path = "integration/saga_manager_multi_source.rs"]
mod saga_manager_multi_source;
#[path = "integration/saga_manager_offset_holds_below_unsettled_record.rs"]
mod saga_manager_offset_holds_below_unsettled_record;
#[path = "integration/saga_manager_offset_is_per_source_and_partition.rs"]
mod saga_manager_offset_is_per_source_and_partition;
#[path = "integration/saga_manager_offset_regression_is_safe.rs"]
mod saga_manager_offset_regression_is_safe;
#[path = "integration/saga_manager_offset_save_failure.rs"]
mod saga_manager_offset_save_failure;
#[path = "integration/saga_manager_passivation.rs"]
mod saga_manager_passivation;
#[path = "integration/saga_manager_props_holds_event_store.rs"]
mod saga_manager_props_holds_event_store;
#[path = "integration/saga_manager_provider_event_store.rs"]
mod saga_manager_provider_event_store;
#[path = "integration/saga_manager_restart_rereads_offset.rs"]
mod saga_manager_restart_rereads_offset;
#[path = "integration/saga_manager_restart_resumes_from_offset.rs"]
mod saga_manager_restart_resumes_from_offset;
#[path = "integration/saga_manager_resume_leaves_deadlines_to_the_index.rs"]
mod saga_manager_resume_leaves_deadlines_to_the_index;
#[path = "integration/saga_manager_single_subscription.rs"]
mod saga_manager_single_subscription;
#[path = "integration/saga_manager_stall_is_source_local.rs"]
mod saga_manager_stall_is_source_local;
#[path = "integration/saga_multi_source_independent_cursors.rs"]
mod saga_multi_source_independent_cursors;
#[path = "integration/saga_multi_source_observation.rs"]
mod saga_multi_source_observation;
#[path = "integration/saga_outbox_dlq_api_removed.rs"]
mod saga_outbox_dlq_api_removed;
#[path = "integration/saga_persisted_envelope_transparency.rs"]
mod saga_persisted_envelope_transparency;
#[path = "integration/saga_producer_takes_components.rs"]
mod saga_producer_takes_components;
#[path = "integration/saga_props_take_their_stores_from_the_system.rs"]
mod saga_props_take_their_stores_from_the_system;
#[path = "integration/saga_provider_event_store.rs"]
mod saga_provider_event_store;
#[path = "integration/saga_replay_appends_nothing.rs"]
mod saga_replay_appends_nothing;
#[path = "integration/saga_replay_with_direct_store.rs"]
mod saga_replay_with_direct_store;
#[path = "integration/saga_schedule_effect.rs"]
mod saga_schedule_effect;
#[path = "integration/saga_schedule_token.rs"]
mod saga_schedule_token;
#[path = "integration/saga_schedule_token_new.rs"]
mod saga_schedule_token_new;
#[path = "integration/saga_scheduled_message_api_removed.rs"]
mod saga_scheduled_message_api_removed;
#[path = "integration/saga_scheduler_fires_into_passivated_instance.rs"]
mod saga_scheduler_fires_into_passivated_instance;
#[path = "integration/saga_scheduler_has_no_recovery_step.rs"]
mod saga_scheduler_has_no_recovery_step;
#[path = "integration/saga_scheduler_resident.rs"]
mod saga_scheduler_resident;
#[path = "integration/saga_snapshot_type_removed.rs"]
mod saga_snapshot_type_removed;
#[path = "integration/saga_spawn_scheduler_api_removed.rs"]
mod saga_spawn_scheduler_api_removed;
#[path = "integration/saga_standalone_spawn_api_removed.rs"]
mod saga_standalone_spawn_api_removed;
#[path = "integration/saga_subscription_poll_interval.rs"]
mod saga_subscription_poll_interval;
#[path = "integration/saga_terminated_skip_on_restart.rs"]
mod saga_terminated_skip_on_restart;
#[path = "integration/saga_trait_correlate.rs"]
mod saga_trait_correlate;
#[path = "integration/saga_trait_snapshot_methods_removed.rs"]
mod saga_trait_snapshot_methods_removed;
#[path = "integration/saga_trait_state_removed.rs"]
mod saga_trait_state_removed;
#[path = "integration/saga_unknown_reserved_record_fails_fast.rs"]
mod saga_unknown_reserved_record_fails_fast;
#[path = "integration/saga_upstream_catchup.rs"]
mod saga_upstream_catchup;
#[path = "integration/saga_upstream_decode_failure_is_skipped.rs"]
mod saga_upstream_decode_failure_is_skipped;
#[path = "integration/saga_with_scheduler_api_removed.rs"]
mod saga_with_scheduler_api_removed;
