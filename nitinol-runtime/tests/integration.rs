//! The integration tests of `nitinol-runtime`, compiled into one binary.

#[path = "common/helpers.rs"]
mod common;

#[path = "integration/ask_with_timeout.rs"]
mod ask_with_timeout;
#[path = "integration/child_processes.rs"]
mod child_processes;
#[path = "integration/combine_drivers.rs"]
mod combine_drivers;
#[path = "integration/dead_letter.rs"]
mod dead_letter;
#[path = "integration/driver.rs"]
mod driver;
#[path = "integration/error_types.rs"]
mod error_types;
#[path = "integration/idle_timeout.rs"]
mod idle_timeout;
#[path = "integration/integration.rs"]
mod integration;
#[path = "integration/lifecycle.rs"]
mod lifecycle;
#[path = "integration/lookup.rs"]
mod lookup;
#[path = "integration/messaging.rs"]
mod messaging;
#[path = "integration/pipe_bounded.rs"]
mod pipe_bounded;
#[path = "integration/pipe_to_self.rs"]
mod pipe_to_self;
#[path = "integration/props_builder_chain.rs"]
mod props_builder_chain;
#[path = "integration/props_capacity_types.rs"]
mod props_capacity_types;
#[path = "integration/props_macro.rs"]
mod props_macro;
#[path = "integration/props_supervision_smart_ctor.rs"]
mod props_supervision_smart_ctor;
#[path = "integration/props_with_driver.rs"]
mod props_with_driver;
#[path = "integration/self_proxy.rs"]
mod self_proxy;
#[path = "integration/spawn.rs"]
mod spawn;
#[path = "integration/spawn_identity.rs"]
mod spawn_identity;
#[path = "integration/spawn_resident.rs"]
mod spawn_resident;
#[path = "integration/spawn_with_driver.rs"]
mod spawn_with_driver;
#[path = "integration/stash.rs"]
mod stash;
#[path = "integration/stream.rs"]
mod stream;
#[path = "integration/stream_props.rs"]
mod stream_props;
#[path = "integration/subscriber_context.rs"]
mod subscriber_context;
#[path = "integration/supervision.rs"]
mod supervision;
#[path = "integration/system_defaults_inherit.rs"]
mod system_defaults_inherit;
#[path = "integration/system_handle_clone.rs"]
mod system_handle_clone;
#[path = "integration/unified_spawn.rs"]
mod unified_spawn;
#[path = "integration/watch.rs"]
mod watch;
