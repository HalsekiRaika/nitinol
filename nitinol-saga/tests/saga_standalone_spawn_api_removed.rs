//! The standalone saga spawn path leaves no surface behind: a user who still
//! names one of its entry points must fail to compile.
//!
//! ```gherkin
//!   Scenario: a reference to a removed entry point is a compile error
//!     Given user code naming SagaSpawn / SagaProps / SagaProxy /
//!           spawn_saga / saga_manager_props / saga_delivery_props /
//!           subscribed_to
//!     When it is compiled against this crate
//!     Then compilation fails — nothing is deprecated and no hidden compatible
//!          shim answers for the name
//! ```
//!
//! `SagaSystemExt::host` is the one system-side verb, `SagaDeliveryExt::deliver`
//! its delivery-host counterpart, and `SagaManagerProps::observing` the one way
//! a declared source is bound. Those are pinned positively by
//! `saga_host_entry_point.rs`, `saga_hosted_props_spawn.rs` and the
//! `persistence_delivery_*.rs` tests; this file pins that the shapes they
//! replace are gone rather than kept beside them.
//!
//! Two mechanisms, each the one that fits what is being settled, both taken
//! from the `*_api_removed.rs` tests already in this directory:
//!
//! 1. **Released type names** — the crate's public namespace and a local
//!    `probe` namespace are both glob-imported and the bare names are used.
//!    Once the crate stops exporting them the names have a single source and
//!    resolve to the probe; while it still exports one the two globs collide
//!    and this binary fails to build. Each probe type carries an `owner` field
//!    the released types never had, so resolving the ambiguity in the crate's
//!    favour does not rescue the build either.
//! 2. **Released methods** — a blanket extension trait claims the name with a
//!    signature the released method could never accept.
//!    - For the three *trait* methods (`spawn_saga`, `saga_manager_props`,
//!      `saga_delivery_props`) the probe claims the same receiver form the
//!      released one took, so while the released trait item exists both
//!      candidates are applicable at the same autoref step — the ambiguity
//!      (E0034) that fails the build. The store below supplies a delivery
//!      checkpoint slot, which is what keeps the `saga_delivery_props`
//!      candidate applicable: a store with no slot would leave the released
//!      method out of scope and the probe resolving while it still existed.
//!    - For the *inherent* `subscribed_to` an inherent method wins method
//!      resolution over a trait one, so while it exists the call below is
//!      checked against *it* and fails on its argument. Once the name is
//!      `observing`, the blanket method is the only candidate and resolves.
//!
//! One removal is settled by a type that cannot be named rather than by an
//! assertion of its own: `ScheduleDelivery::ToTheInstance` is crate-private, so
//! no downstream build can name it. What it stood for — a deadline addressed to
//! the instance process itself — is settled behaviourally instead, by
//! `saga_scheduler_fires_into_passivated_instance.rs`: the one surviving route
//! restores a passivated instance, which the withdrawn one could not do.
//!
//! Run with:
//! `cargo test -p nitinol-saga --test saga_standalone_spawn_api_removed`.

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;

use nitinol_eventsource::codec::Codec;
use nitinol_eventsource::system::EventSourceSystem;
use nitinol_eventsource::Event;
use nitinol_persistence::provider::PersistenceProvider;
use nitinol_persistence::store::{EventStore, InMemoryEventStore, InMemoryStore};
use nitinol_persistence::{EventType, Family, TypeName};
use nitinol_runtime::ProcessSystem;

/// What a downstream crate is free to define once the names are released.
mod probe {
    /// What a released name means to a downstream crate: nothing the framework
    /// decides. The `owner` field is what no released type ever had.
    macro_rules! released_type {
        ($($name:ident),+ $(,)?) => {
            $(
                pub struct $name {
                    pub owner: &'static str,
                }
            )+
        };
    }

    released_type!(SagaSpawn, SagaProps, SagaProxy);

    /// What a probed call answers with once the framework stops claiming the
    /// name.
    pub struct Declined {
        pub label: &'static str,
    }

    /// Claims the two withdrawn `SagaSystemExt` entry points.
    ///
    /// `&self` on purpose: both released methods took `&self`, and method
    /// resolution walks the autoref steps in order rather than looking at
    /// arity. Claiming the same receiver form makes both candidates applicable
    /// at the same step, which is the ambiguity that fails the build while the
    /// released trait item is still there.
    pub trait NoStandaloneSystemEntry {
        fn spawn_saga(&self, label: &'static str) -> Declined {
            Declined { label }
        }

        fn saga_manager_props(&self, label: &'static str) -> Declined {
            Declined { label }
        }
    }

    impl<T> NoStandaloneSystemEntry for T {}

    /// Claims the withdrawn `SagaDeliveryExt` entry point, for the same reason
    /// and in the same receiver form.
    pub trait NoSagaDeliveryPropsEntry {
        fn saga_delivery_props(&self, label: &'static str) -> Declined {
            Declined { label }
        }
    }

    impl<T> NoSagaDeliveryPropsEntry for T {}

    /// Claims the withdrawn source-binding verb on the saga builders.
    pub trait NoSubscribedToVerb {
        fn subscribed_to(&self, label: &'static str) -> Declined {
            Declined { label }
        }
    }

    impl<T> NoSubscribedToVerb for T {}
}

// The crate glob is the probe, not a source of names: it contributes nothing
// only while the crate exports none of the released names. The moment it
// exports one, this glob collides with `probe::*` and the use sites below stop
// resolving.
#[allow(unused_imports)]
use nitinol_saga::*;
// A glob, not a named import: a named import would shadow the crate's export
// instead of colliding with it, and the collision is the whole mechanism.
#[allow(unused_imports)]
use probe::*;

const PROBE_OWNER: &str = "downstream-probe";
const PROBE_LABEL: &str = "downstream-label";

// A minimal saga, so a builder value and a system exist to call the probed
// items on.

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
        Family::new("saga_standalone_spawn_api_removed"),
        TypeName::new("Signal"),
    );
    type Codec = UnitCodec;
}

#[derive(Clone)]
struct Noted;

impl Event for Noted {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("saga_standalone_spawn_api_removed"),
        TypeName::new("Noted"),
    );
    type Codec = UnitCodec;
}

#[derive(SagaSubscribe)]
enum Observed {
    Signal(Signal),
}

/// Correlation rule of [`ProbeSaga`]: the single process instance every
/// upstream event in this file belongs to.
const PROBE_SAGA_ID: &str = "standalone-spawn-api-removed";

struct ProbeSaga;

#[async_trait]
impl Saga for ProbeSaga {
    type Subscribe = Observed;
    type Event = Noted;
    type Error = std::convert::Infallible;

    fn correlate(_input: &Self::Subscribe) -> Option<SagaId> {
        Some(SagaId::new(PROBE_SAGA_ID))
    }

    async fn handle(
        &mut self,
        _input: Self::Subscribe,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        Ok(SagaEffect::None)
    }
}

fn store() -> Arc<dyn EventStore> {
    Arc::new(InMemoryEventStore::default())
}

fn manager_props() -> SagaManagerProps<ProbeSaga> {
    SagaManagerProps::<ProbeSaga>::new(store(), |_components| ProbeSaga)
}

/// A system over a store that supplies every slot, including the delivery
/// checkpoint slot the withdrawn `saga_delivery_props` required.
async fn system() -> EventSourceSystem<InMemoryStore> {
    EventSourceSystem::builder(ProcessSystem::new().await)
        .with_persistence(PersistenceProvider::new(InMemoryStore::default()))
        .build()
}

/// Given both namespaces are glob-imported, When each bare standalone-spawn
/// name is used, Then every one of them denotes the downstream probe definition
/// — a saga started through `host` is a manager instance, so there is no
/// separate builder, no separate typestate carrier, and no separate handle.
#[test]
fn the_standalone_spawn_type_names_belong_to_downstream_definitions() {
    let owners = [
        SagaSpawn { owner: PROBE_OWNER }.owner,
        SagaProps { owner: PROBE_OWNER }.owner,
        SagaProxy { owner: PROBE_OWNER }.owner,
    ];

    assert!(
        owners.iter().all(|owner| *owner == PROBE_OWNER),
        "every standalone-spawn name must be released by `nitinol-saga`: a saga \
         whose `correlate` always answers the same id is the degenerate case of \
         the manager rather than a second mechanism, so `SagaManagerProps` and \
         `SagaManagerProxy` are the only builder and handle left; got {owners:?}"
    );
}

/// Given a blanket extension trait that claims the two withdrawn
/// `SagaSystemExt` names, When each is called on a system, Then both resolve to
/// the extension — `host` is the one system-side spawn verb.
#[tokio::test]
async fn the_withdrawn_system_entry_names_belong_to_downstream_definitions() {
    let system = system().await;

    assert_eq!(
        (
            system.spawn_saga(PROBE_LABEL).label,
            system.saga_manager_props(PROBE_LABEL).label,
        ),
        (PROBE_LABEL, PROBE_LABEL),
        "`SagaSystemExt` must offer neither `spawn_saga` nor \
         `saga_manager_props`: what is left to configure at a wiring site is \
         the binding of each declared source and the instances' passivation, \
         which is one builder's worth — so there is one verb, `host`"
    );
}

/// Given a blanket extension trait that claims the withdrawn
/// `saga_delivery_props` name, When it is called on a system whose store
/// supplies the delivery checkpoint slot, Then it resolves to the extension —
/// `deliver` is the delivery host's verb.
#[tokio::test]
async fn the_withdrawn_delivery_entry_name_belongs_to_downstream_definitions() {
    let system = system().await;

    assert_eq!(
        system.saga_delivery_props(PROBE_LABEL).label,
        PROBE_LABEL,
        "`SagaDeliveryExt` must not name its entry point `saga_delivery_props` \
         any more: the delivery host is started by `deliver`, symmetric with \
         `host`, and the typestate that refuses a provider with no checkpoint \
         slot moves with it"
    );
}

/// Given a blanket extension trait that claims the `subscribed_to` name, When
/// it is called on the manager builder, Then it resolves to the extension — a
/// declared source is bound by `observing`.
#[test]
fn the_subscribed_to_verb_belongs_to_downstream_definitions() {
    assert_eq!(
        manager_props().subscribed_to(PROBE_LABEL).label,
        PROBE_LABEL,
        "`SagaManagerProps` must bind a declared source with `observing`, not \
         with `subscribed_to`: one vocabulary for the one thing a wiring site \
         still says about a source"
    );
}
