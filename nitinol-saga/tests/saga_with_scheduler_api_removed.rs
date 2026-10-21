//! The hosting machinery owns the scheduler, so the builder does not take one
//! and no user can hold one.
//!
//! Two names are settled here:
//!
//! 1. **`SchedulerProxy` is no longer exported.**  Observed the way
//!    `saga_spawn_scheduler_api_removed.rs` observes its own: the crate's public
//!    namespace and a local `probe` namespace are both glob-imported and the
//!    bare name is used.  Once the crate stops exporting it the name has a
//!    single source and resolves to the probe; while it still exports one the
//!    two globs collide and this binary fails to build.  The probe type carries
//!    a field the released one never had, so resolving the ambiguity in the
//!    crate's favour does not rescue the build either.
//! 2. **`with_scheduler` is no longer a builder method.**  Observed by a
//!    blanket extension trait: an inherent method wins method resolution over a
//!    trait one, so while `SagaManagerProps::with_scheduler` exists the call
//!    below is checked against *it* and fails on its argument.  Once it is gone
//!    the blanket method is the only candidate and resolves.
//!    `SagaManagerProps` is the one saga builder there is to claim the name on:
//!    a saga whose `correlate` always answers the same id is the degenerate
//!    case of the manager, so there is no second builder to probe.
//!
//! `SchedulerProcess` stays exported on purpose: it is the type key the host
//! resolves the resident timer core by, and the positive half — a saga whose
//! alphabet declares a deadline getting that core without any wiring — is
//! `saga_host_owns_scheduler.rs`.
//!
//! ## Why a user must not hold a proxy
//!
//! A live proxy in user hands makes "arm a timer without writing the fact"
//! expressible, and that is the one state a deadline must never be in: a timer
//! no stream knows about does not survive a restart, and a restart would not
//! know it was lost.  Taking the proxy out of the public surface is what leaves
//! effect → append → arm as the only path.
//!
//! Run with:
//! `cargo test -p nitinol-saga --test saga_with_scheduler_api_removed`.

use async_trait::async_trait;
use bytes::Bytes;

use nitinol::saga::SagaSubscribe;
use nitinol_eventsource::codec::Codec;
use nitinol_eventsource::Event;
use nitinol_persistence::{EventType, Family, TypeName};
use nitinol_saga::{Saga, SagaContext, SagaEffect, SagaId};

/// What a downstream crate is free to define once the name is released: a host
/// that owns its timer core naming the handle for its own wiring.
mod probe {
    pub struct SchedulerProxy {
        pub owner: &'static str,
    }

    /// What `with_scheduler` means to a downstream crate once the builder stops
    /// claiming the name: nothing the framework decides.
    pub struct Declined {
        pub label: &'static str,
    }

    pub trait TheHostOwnsTheScheduler {
        fn with_scheduler(&self, label: &'static str) -> Declined {
            Declined { label }
        }
    }

    impl<T> TheHostOwnsTheScheduler for T {}
}

// The crate glob is the probe, not a source of names: it contributes nothing
// only while the crate exports no `SchedulerProxy`.  The moment it exports one,
// this glob collides with `probe::*` and the use below stops resolving.
#[allow(unused_imports)]
use nitinol_saga::*;
// A glob, not a named import: a named import would shadow the crate's export
// instead of colliding with it, and the collision is the whole mechanism.
#[allow(unused_imports)]
use probe::*;

const PROBE_OWNER: &str = "downstream-probe";
const PROBE_LABEL: &str = "downstream-label";

// A minimal saga, so a builder value exists to call the probed method on.

struct UnitCodec;

impl Codec<Signal> for UnitCodec {
    type Error = std::convert::Infallible;

    fn encode(_value: &Signal) -> Result<Bytes, Self::Error> {
        Ok(Bytes::new())
    }

    fn decode(_payload: &[u8]) -> Result<Signal, Self::Error> {
        Ok(Signal)
    }
}

impl Codec<Noted> for UnitCodec {
    type Error = std::convert::Infallible;

    fn encode(_value: &Noted) -> Result<Bytes, Self::Error> {
        Ok(Bytes::new())
    }

    fn decode(_payload: &[u8]) -> Result<Noted, Self::Error> {
        Ok(Noted)
    }
}

#[derive(Clone)]
struct Signal;

impl Event for Signal {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("saga_with_scheduler_removed"),
        TypeName::new("Signal"),
    );
    type Codec = UnitCodec;
}

#[derive(Clone)]
struct Noted;

impl Event for Noted {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("saga_with_scheduler_removed"),
        TypeName::new("Noted"),
    );
    type Codec = UnitCodec;
}

#[derive(SagaSubscribe)]
enum Observed {
    Signal(Signal),
}

struct ProbeSaga;

#[async_trait]
impl Saga for ProbeSaga {
    type Subscribe = Observed;
    type Event = Noted;
    type Error = std::convert::Infallible;

    fn correlate(_input: &Self::Subscribe) -> Option<SagaId> {
        Some(SagaId::new("with-scheduler-removed"))
    }

    async fn handle(
        &mut self,
        _input: Self::Subscribe,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        Ok(SagaEffect::None)
    }
}

/// Given both namespaces are glob-imported, When the bare `SchedulerProxy` name
/// is used, Then it denotes the downstream probe definition.
#[test]
fn the_scheduler_proxy_name_belongs_to_downstream_definitions() {
    let proxy = SchedulerProxy { owner: PROBE_OWNER };

    assert_eq!(
        proxy.owner, PROBE_OWNER,
        "`SchedulerProxy` must not be exported by `nitinol-saga` any more: a \
         user who can hold one can arm a timer without writing the fact"
    );
}

/// Given a blanket extension trait that claims the `with_scheduler` name,
/// When it is called on the saga builder,
/// Then it resolves to the extension — the builder has no inherent method of
/// that name to win resolution.
#[test]
fn the_with_scheduler_name_belongs_to_downstream_definitions() {
    let manager_props = SagaManagerProps::<ProbeSaga>::new(|_components| ProbeSaga);

    assert_eq!(
        manager_props.with_scheduler(PROBE_LABEL).label,
        PROBE_LABEL,
        "`SagaManagerProps` must not take a scheduler: the host resolves the \
         resident timer core from the saga's own alphabet"
    );
}

/// The positive counterpart: the resident's type key stays exported, because
/// that is what the host resolves the one timer core by.
#[test]
fn the_scheduler_process_type_key_stays_exported() {
    fn names_a_resident<P: nitinol_runtime::process::Resident>() {}

    names_a_resident::<nitinol_saga::SchedulerProcess>();
}
