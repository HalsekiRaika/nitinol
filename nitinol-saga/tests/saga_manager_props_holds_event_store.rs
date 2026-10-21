//! The manager builder and the subscription constructors name no journal at
//! all: every store a saga touches comes from the system it is spawned on.
//!
//! ```gherkin
//!   Scenario: a manager built from nothing but a producer
//!     Given a system over a store that supplies every slot
//!     When a manager is built with no journal argument and given a store-wide
//!          subscription
//!     Then it spawns on that system
//!
//!   Scenario: neither constructor receives a journal
//!     Given SagaManagerProps::new, Subscription::stream and
//!           Subscription::global
//!     When their signatures are named
//!     Then the producer, the stream key and nothing else are what they take
//! ```
//!
//! `SagaManagerProps::new` is the props-first entry point, and the argument
//! shape is the whole subject here: the producer arrives alone, so the journal
//! its instances persist onto, the journal its sources are observed from and
//! the slot its delivery host records in are all the ones the system handed to
//! `spawn` distributes. A deployment reaching a second destination builds a
//! second system over the supplier that holds it. What the manager then does
//! with that store is pinned in `saga_hosted_props_spawn.rs`.

#[path = "common/helpers.rs"]
mod common;
use common::JsonCodec;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use nitinol_eventsource::{system::EventSourceSystem, Event};
use nitinol_persistence::provider::PersistenceProvider;
use nitinol_persistence::store::InMemoryStore;
use nitinol_persistence::{AggregateId, EventType, Family, TypeName};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{
    Saga, SagaComponents, SagaContext, SagaEffect, SagaId, SagaManagerProps, SagaSubscribe,
    Subscription,
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

/// Given a system over a store that supplies every slot, When a manager is
/// built from nothing but a producer, Then it spawns on that system.
#[tokio::test]
async fn manager_props_spawn_takes_every_store_from_the_system() {
    let system = a_system().await;

    let _manager = SagaManagerProps::<TrivialSaga>::new(|_| TrivialSaga)
        .observing::<Triggered>(Subscription::global())
        .spawn(&system)
        .await;
}

/// The manager builder receives the producer and nothing else.
///
/// Not a `#[test]` — the compiler enforces the signature at build time. A
/// regression that put a journal back in front of the producer is a compile
/// error here, which is the whole claim: the props carry no store and take it
/// from the system `spawn` is given.
/// The producer a manager is built from — the one argument `new` takes.
type Producer = fn(SagaComponents<TrivialSaga>) -> TrivialSaga;

#[allow(dead_code)]
fn _assert_sig_manager_props_new_receives_only_a_producer() {
    let _: fn(Producer) -> SagaManagerProps<TrivialSaga> = SagaManagerProps::<TrivialSaga>::new;
}

/// Neither subscription constructor receives a journal.
///
/// Not a `#[test]` — the compiler enforces the signatures at build time.
/// `Subscription::stream` names a stream key however the caller holds one — an
/// `AggregateId`, another saga's `SagaId`, a bare `&str` — and
/// `Subscription::global` names nothing at all, because the journal both read
/// is the one the system distributes.
#[allow(dead_code)]
fn _assert_sig_the_subscription_constructors_receive_no_journal() {
    let _: fn(&AggregateId) -> Subscription = Subscription::stream;
    let _: fn(&SagaId) -> Subscription = Subscription::stream;
    let _: fn(&str) -> Subscription = Subscription::stream;

    let _: fn() -> Subscription = Subscription::global;
}
