//! Every spawn entry point builds the saga from the components it is handed.
//!
//! ```gherkin
//!   Scenario: capabilities arrive as an argument, at every entry point
//!     Given a saga that needs no capability
//!     When it is spawned through each of the entry points
//!     Then its producer is `|_| Saga::default()` and nothing else is required
//! ```
//!
//! ## Why the producer takes an argument
//!
//! A capability a saga needs — its own time, today — used to reach it by being
//! captured in the producer closure, which meant the capability had to be
//! obtained *before* the builder that knows how to resolve it existed.  Handing
//! the components to the producer reverses that: the framework resolves what it
//! owns, then calls the producer with it, so a saga's dependencies arrive the
//! way any other function's do.
//!
//! The counterpart is that a saga needing nothing must not pay for the change.
//! `|_| TrivialSaga::default()` is the whole producer below — the components are
//! offered and ignored, at every entry point.
//!
//! ## API contract assumed by this file
//!
//! Both entry points take `producer: impl Fn(SagaComponents<S>) -> S`:
//!
//! 1. `SagaSystemExt::host::<S>(producer)`
//! 2. `SagaManagerProps::new(store, producer)`
//!
//! Run with:
//! `cargo test -p nitinol-saga --test saga_producer_takes_components`.

#[path = "common/helpers.rs"]
mod common;
use common::JsonCodec;

use std::sync::Mutex;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;
use serde::{Deserialize, Serialize};

use nitinol::saga::SagaSubscribe;
use nitinol_eventsource::system::EventSourceSystem;
use nitinol_eventsource::Event;
use nitinol_persistence::provider::{CustomStore, PersistenceProvider};
use nitinol_persistence::store::{InMemoryEventStore, InMemoryOffsetStore, Journal};
use nitinol_persistence::{AppendingEvent, EventType, Family, TypeName};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{
    Saga, SagaContext, SagaEffect, SagaId, SagaManagerProps, SagaSubscriptionExt, SagaSystemExt,
    Subscription,
};

// Domain

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Ping {
    at: String,
}

impl Event for Ping {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("producer_components"), TypeName::new("Ping"));
    type Codec = JsonCodec;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Pong {
    at: String,
}

impl Event for Pong {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("producer_components"), TypeName::new("Pong"));
    type Codec = JsonCodec;
}

#[derive(SagaSubscribe)]
enum Pinged {
    Ping(Ping),
}

/// Which entry points were observed handling their own record.
///
/// A `static` because the producer below is `|_| TrivialSaga::default()`: the
/// whole point is that it captures nothing, so there is nothing for an `Arc` to
/// be threaded through.
static HANDLED: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Needs no capability, so it needs nothing from the components.
#[derive(Default)]
struct TrivialSaga;

#[async_trait]
impl Saga for TrivialSaga {
    type Subscribe = Pinged;
    type Event = Pong;
    type Error = std::convert::Infallible;

    fn correlate(input: &Self::Subscribe) -> Option<SagaId> {
        let Pinged::Ping(ping) = input;
        Some(SagaId::new(ping.at.clone()))
    }

    async fn handle(
        &mut self,
        input: Self::Subscribe,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        let Pinged::Ping(ping) = input;
        HANDLED
            .lock()
            .expect("handled registry must not be poisoned")
            .push(ping.at.clone());
        Ok(SagaEffect::persist(Pong { at: ping.at }))
    }
}

// Helpers

/// Each entry point gets its own upstream stream key, which is also the saga id
/// `correlate` names, so the observations below cannot be confused.
const THROUGH_MANAGER_PROPS: &str = "producer-manager-props";
const THROUGH_HOST: &str = "producer-host";

async fn append_ping(store: &Journal, at: &str) {
    store
        .append(
            at,
            vec![AppendingEvent {
                sequence: 1,
                event_type: Ping::EVENT_TYPE,
                payload: serde_json::to_vec(&Ping { at: at.to_owned() })
                    .map(Bytes::from)
                    .expect("encode Ping must succeed"),
                occurred_at: jiff::Timestamp::now(),
            }],
        )
        .await
        .expect("append Ping must succeed");
}

const BARRIER_TIMEOUT: Duration = Duration::from_secs(8);
const POLL_INTERVAL: Duration = Duration::from_millis(25);

async fn wait_for_all(expected: &[&str]) -> Vec<String> {
    let deadline = Instant::now() + BARRIER_TIMEOUT;
    loop {
        let mut current = HANDLED
            .lock()
            .expect("handled registry must not be poisoned")
            .clone();
        current.sort();
        current.dedup();
        if expected
            .iter()
            .all(|at| current.iter().any(|seen| seen == at))
        {
            return current;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {expected:?} to be handled; saw {current:?}"
        );
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

// Tests

/// Given a saga that needs no capability,
/// When it is spawned through every entry point with `|_| Saga::default()`,
/// Then each spawn runs and handles its own record.
///
/// One test, because the claim is about both entry points sharing one producer
/// shape; splitting it would put the same `static` registry under concurrent
/// tests.
// `|_| TrivialSaga::default()` spelled out *is* what this file pins — a saga
// needing no capability pays nothing for the components argument. Clippy's
// shorter form for a unit struct would stop pinning it.
#[allow(clippy::default_constructed_unit_structs)]
#[tokio::test]
async fn every_entry_point_builds_the_saga_from_the_components_it_is_handed() {
    let ps = ProcessSystem::new().await;
    let own_store = Journal::from(InMemoryEventStore::default());
    let upstream_store = Journal::from(InMemoryEventStore::default());

    // Entry point 1 — `SagaManagerProps`, built on a store of its own.
    append_ping(&upstream_store, THROUGH_MANAGER_PROPS).await;
    let _through_manager_props =
        SagaManagerProps::<TrivialSaga>::new(own_store.clone(), |_components| {
            TrivialSaga::default()
        })
        .observing::<Ping>(Subscription::stream(&upstream_store, THROUGH_MANAGER_PROPS))
        .spawn(&ps)
        .await;

    // Entry point 2 — `host` on a system, journalling onto the store the
    // provider supplies.
    let system = EventSourceSystem::builder(ps.clone())
        .with_persistence(PersistenceProvider::new(
            CustomStore::builder()
                .eventstore(InMemoryEventStore::default())
                .saga_offsets(InMemoryOffsetStore::default())
                .build(),
        ))
        .build();
    let provider_store = system.persistence().event_store();
    append_ping(&provider_store, THROUGH_HOST).await;

    let _through_host = system
        .host::<TrivialSaga>(|_components| TrivialSaga::default())
        .observing::<Ping>(system.subscription(THROUGH_HOST))
        .spawn()
        .await;

    let expected = [THROUGH_HOST, THROUGH_MANAGER_PROPS];
    let seen = wait_for_all(&expected).await;

    assert_eq!(
        seen,
        expected
            .iter()
            .map(|at| (*at).to_owned())
            .collect::<Vec<_>>(),
        "every entry point must take a `Fn(SagaComponents<S>) -> S` producer, and \
         a saga that needs no capability must be spawnable with \
         `|_| Saga::default()` at each of them"
    );
}
