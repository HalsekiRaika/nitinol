//! The store-bound manager builder and the subscription constructors receive
//! the journal as `impl Into<Journal>`.
//!
//! ```gherkin
//!   Scenario: a manager journalling into a destination of its own
//!     Given a journal held as a backend handle, as a Journal, or as a
//!           reference to one
//!     When a manager is built on it and given a store-wide subscription
//!     Then it spawns on the process system it is handed
//!
//!   Scenario: the subscription constructors receive the same three forms
//!     Given Subscription::stream and Subscription::global
//!     When their signatures are named
//!     Then both receive a journal however the caller holds one
//! ```
//!
//! `SagaManagerProps::new` is what remains for the deployment that journals
//! into another destination, and the argument shape is the whole subject here:
//! the journal arrives as `impl Into<Journal>`, so a deployment names the
//! backend it constructed, the handle its supplier answered with, or a reference
//! to one it has to keep — and none of the three needs wrapping to be named
//! twice. What the manager then does with that store is pinned in
//! `saga_hosted_props_spawn.rs`.

#[path = "common/helpers.rs"]
mod common;
use common::JsonCodec;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use nitinol_eventsource::{system::EventSourceSystem, Event};
use nitinol_persistence::provider::PersistenceProvider;
use nitinol_persistence::store::{InMemoryEventStore, InMemoryStore, Journal};
use nitinol_persistence::{EventType, Family, TypeName};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{
    Saga, SagaContext, SagaEffect, SagaId, SagaManagerProps, SagaSubscribe, Subscription,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Triggered;

impl Event for Triggered {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("saga.direct"), TypeName::new("Triggered"));
    type Codec = JsonCodec;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Recorded;

impl Event for Recorded {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("saga.direct"), TypeName::new("Recorded"));
    type Codec = JsonCodec;
}

/// Correlation rule of [`TrivialSaga`]: the single process instance every
/// `Triggered` belongs to.
const TRIVIAL_SAGA_ID: &str = "saga-manager-props-direct";

/// The whole alphabet this saga reacts to: one observed source, no deadline.
#[derive(SagaSubscribe)]
enum Observed {
    Triggered(Triggered),
}

#[derive(Default)]
struct TrivialSaga;

#[async_trait]
impl Saga for TrivialSaga {
    type Subscribe = Observed;
    type Event = Recorded;
    type Error = std::convert::Infallible;

    fn correlate(_input: &Self::Subscribe) -> Option<SagaId> {
        Some(SagaId::new(TRIVIAL_SAGA_ID))
    }

    async fn handle(
        &mut self,
        _input: Self::Subscribe,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        Ok(SagaEffect::None)
    }
}

/// A system whose supplier holds every slot, which is what the manager's host
/// is resolved from.
async fn a_system() -> EventSourceSystem<InMemoryStore> {
    EventSourceSystem::builder(ProcessSystem::new().await)
        .with_persistence(PersistenceProvider::new(InMemoryStore::default()))
        .build()
}

/// Given the saga's journal and the upstream one are held as backend handles,
/// When a manager is built on them, Then it spawns.
#[tokio::test]
async fn manager_props_spawns_with_a_backend_handle() {
    let system = a_system().await;

    let _manager =
        SagaManagerProps::<TrivialSaga>::new(InMemoryEventStore::default(), |_| TrivialSaga)
            .observing::<Triggered>(Subscription::global(InMemoryEventStore::default()))
            .spawn(system.process_system())
            .await;
}

/// Given both are held as `Journal`s, When a manager is built on them, Then it
/// spawns.
#[tokio::test]
async fn manager_props_spawns_with_a_journal() {
    let system = a_system().await;
    let saga_journal = Journal::from(InMemoryEventStore::default());
    let upstream_journal = Journal::from(InMemoryEventStore::default());

    let _manager = SagaManagerProps::<TrivialSaga>::new(saga_journal, |_| TrivialSaga)
        .observing::<Triggered>(Subscription::global(upstream_journal))
        .spawn(system.process_system())
        .await;
}

/// Given both are held as journals the caller keeps, When a manager is built on
/// references to them, Then it spawns.
///
/// This is the form a deployment reaches for when it reads the same journals
/// back itself, and it is the one the withdrawn by-reference signatures over a
/// framework-shared handle used to be the only way to express.
#[tokio::test]
async fn manager_props_spawns_with_a_reference_to_a_journal() {
    let system = a_system().await;
    let saga_journal = Journal::from(InMemoryEventStore::default());
    let upstream_journal = Journal::from(InMemoryEventStore::default());

    let _manager = SagaManagerProps::<TrivialSaga>::new(&saga_journal, |_| TrivialSaga)
        .observing::<Triggered>(Subscription::global(&upstream_journal))
        .spawn(system.process_system())
        .await;

    // The caller still holds both journals after naming them, which is what
    // taking a reference is for.
    let _still_held = (saga_journal, upstream_journal);
}

/// Both subscription constructors receive the journal in all three forms.
///
/// Not a `#[test]` — the compiler enforces the signatures at build time.
/// `Subscription::stream` keys the upstream stream separately, so the two
/// generic positions are pinned together here: a regression that fixed either
/// one to a shared handle is a compile error.
///
/// The by-reference forms are named at one lifetime `'j`, because the handle is
/// a type parameter: `impl Into<Journal>` is settled where the constructor is
/// instantiated, so `&'j Journal` is one of the forms rather than a family of
/// them.
#[allow(dead_code)]
fn _assert_sig_the_subscription_constructors_receive_any_form_of_a_journal<'j>(_held: &'j Journal) {
    let _: fn(InMemoryEventStore, &str) -> Subscription = Subscription::stream;
    let _: fn(Journal, &str) -> Subscription = Subscription::stream;
    let _: fn(&'j Journal, &str) -> Subscription = Subscription::stream;

    let _: fn(InMemoryEventStore) -> Subscription = Subscription::global;
    let _: fn(Journal) -> Subscription = Subscription::global;
    let _: fn(&'j Journal) -> Subscription = Subscription::global;
}
