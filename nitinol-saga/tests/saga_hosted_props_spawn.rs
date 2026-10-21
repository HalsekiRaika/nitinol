//! Saga props taken from a system carry where they start.
//!
//! ```gherkin
//!   Scenario: a saga hosted through the system's own entry point
//!     Given a saga hosted from a system
//!     When the caller spawns it without naming a process system
//!     Then the host process is running on the system's process system
//!     And the instances it spawns journal onto the store that system was built
//!         on
//!
//!   Scenario: props built directly name the system they start on
//!     Given manager props built from nothing but a producer
//!     When the caller spawns them on a system
//!     Then that system's process system is where they start
//!     And the instances they spawn journal onto the store it was built on
//! ```
//!
//! There is one hosted entry point and so one hosted clause: `host` always
//! starts a manager, and a saga whose `correlate` answers a single id is that
//! manager with one instance rather than a second kind of spawn.
//!
//! Where a process is running is read from the system's own process registry:
//! the identifier the proxy carries is looked up on the process system the
//! props came from. A hosted spawn that started the process somewhere else
//! would hand back a working proxy that this lookup does not find — and the
//! journal clause alone would not notice, because a store is shared by
//! whichever process reaches it.
//!
//! The journal clause is read through the store handle this file constructed,
//! rather than through anything the system answers with, so a spawn that built
//! a journal of its own is a spawn that leaves nothing behind here.
//!
//! The delivery host's hosted spawn is fixed where its recovery already is,
//! in `persistence_delivery_without_projector.rs`.

use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};

use nitinol_eventsource::codec::Codec;
use nitinol_eventsource::system::EventSourceSystem;
use nitinol_eventsource::Event;
use nitinol_persistence::provider::{PersistenceProvider, Store};
use nitinol_persistence::store::{InMemoryStore, Journal};
use nitinol_persistence::{AggregateId, AppendingEvent, EventType, Family, LoadQuery, TypeName};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{
    Saga, SagaContext, SagaEffect, SagaId, SagaManagerProps, SagaSubscribe, SagaSystemExt,
    Subscription,
};

// Fixtures

struct JsonCodec;

impl<E: Serialize + for<'de> Deserialize<'de>> Codec<E> for JsonCodec {
    type Error = serde_json::Error;

    fn encode(event: &E) -> Result<Bytes, Self::Error> {
        serde_json::to_vec(event).map(Bytes::from)
    }

    fn decode(payload: &[u8]) -> Result<E, Self::Error> {
        serde_json::from_slice(payload)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct OrderPlaced {
    sku: String,
}

impl Event for OrderPlaced {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("saga.hosted.props"),
        TypeName::new("OrderPlaced"),
    );
    type Codec = JsonCodec;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Noted {
    sku: String,
}

impl Event for Noted {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("saga.hosted.props"), TypeName::new("Noted"));
    type Codec = JsonCodec;
}

/// The whole alphabet this saga reacts to: one observed source, no deadline.
#[derive(SagaSubscribe)]
enum Observed {
    Order(OrderPlaced),
}

/// The correlation every observed order falls under.
const SAGA_ID: &str = "saga-hosted-props";

/// A saga that journals what it observed and nothing else.
///
/// It asks for no capability, so `|_| NotingSaga` is the whole producer — which
/// is what leaves the store the journal landed in as the only thing these
/// clauses read.
struct NotingSaga;

#[async_trait]
impl Saga for NotingSaga {
    type Subscribe = Observed;
    type Event = Noted;
    type Error = std::convert::Infallible;

    fn correlate(_input: &Self::Subscribe) -> Option<SagaId> {
        Some(SagaId::new(SAGA_ID))
    }

    async fn handle(
        &mut self,
        input: Self::Subscribe,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        let Observed::Order(event) = input;
        Ok(SagaEffect::persist(Noted { sku: event.sku }))
    }
}

// Helpers

const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
const PROBE_INTERVAL: Duration = Duration::from_millis(20);

async fn append_order_placed(store: &Journal, key: &AggregateId, sequence: u64, sku: &str) {
    let payload = serde_json::to_vec(&OrderPlaced {
        sku: sku.to_owned(),
    })
    .map(Bytes::from)
    .expect("encoding a fixture order succeeds");
    store
        .append(
            key.as_str(),
            vec![AppendingEvent {
                sequence,
                event_type: OrderPlaced::EVENT_TYPE,
                payload,
                occurred_at: jiff::Timestamp::now(),
            }],
        )
        .await
        .expect("appending a fixture order succeeds");
}

/// The SKUs on the saga's own stream, decoded with the codec `Noted` names.
async fn noted_skus(store: &Journal, saga_id: &SagaId) -> Vec<String> {
    let loaded: Vec<_> = store
        .load(LoadQuery::by_stream(saga_id))
        .await
        .expect("loading the saga's own stream succeeds")
        .try_collect()
        .await
        .expect("collecting the saga's own stream succeeds");
    loaded
        .iter()
        .filter(|event| event.event_type == Noted::EVENT_TYPE)
        .map(|event| {
            serde_json::from_slice::<Noted>(&event.payload)
                .expect("the saga's own event is encoded with the codec its type names")
                .sku
        })
        .collect()
}

/// Wait until the saga's own stream carries `want` of its events, then answer
/// their SKUs.
async fn wait_for_noted(
    store: &Journal,
    saga_id: &SagaId,
    want: usize,
    label: &str,
) -> Vec<String> {
    let deadline = std::time::Instant::now() + PROBE_TIMEOUT;
    loop {
        let skus = noted_skus(store, saga_id).await;
        if skus.len() >= want {
            return skus;
        }
        if std::time::Instant::now() >= deadline {
            panic!("{label}: the saga's own stream carries {skus:?}, wanted {want} of them");
        }
        tokio::time::sleep(PROBE_INTERVAL).await;
    }
}

// The system's own entry point

/// A saga hosted from a system spawns with no process system named, runs on
/// that system's process system, and has its instances journal onto the store
/// it was built on.
#[tokio::test]
async fn a_saga_hosted_from_a_system_runs_on_that_system() {
    // Given
    let store = InMemoryStore::default();
    let system = EventSourceSystem::builder(ProcessSystem::new().await)
        .with_persistence(PersistenceProvider::new(store.clone()))
        .build();
    let saga_id = SagaId::new(SAGA_ID);
    // One journal: the order stream the saga observes and the saga's own stream
    // are tenants of the store the system was built on, kept apart by key.
    let upstream = store.events();
    let order_id = AggregateId::new("saga-hosted-props-order");

    // When: nothing but the host builder is named here
    let host = system
        .host::<NotingSaga>(|_components| NotingSaga)
        .observing::<OrderPlaced>(Subscription::stream(&order_id))
        .spawn()
        .await;

    // Then: the process the proxy names is one this system is running
    assert!(
        system
            .process_system()
            .lookup(host.manager().pid())
            .await
            .is_some(),
        "a saga hosted from a system runs on that system's process system, so \
         the system's own registry is where its process is found"
    );

    // And: its instances' own events are on the store the system was built on
    append_order_placed(&upstream, &order_id, 1, "SKU-HOSTED").await;
    assert_eq!(
        wait_for_noted(
            &store.events(),
            &saga_id,
            1,
            "the hosted saga must journal what it observed"
        )
        .await,
        vec!["SKU-HOSTED".to_owned()],
        "the journal a hosted spawn resolves is the store the system was built \
         on, not one of its own"
    );
}

// The entry point built directly

/// Manager props built from nothing but a producer start on the system they are
/// spawned on.
///
/// The props-first form names no store of its own, so the system handed to
/// `spawn` is both where the manager runs and where its instances journal —
/// which is what the hosted form carries already rather than taking as an
/// argument.
#[tokio::test]
async fn manager_props_built_directly_start_on_the_system_they_are_given() {
    // Given
    let store = InMemoryStore::default();
    let system = EventSourceSystem::builder(ProcessSystem::new().await)
        .with_persistence(PersistenceProvider::new(store.clone()))
        .build();
    let saga_id = SagaId::new(SAGA_ID);
    let journal = store.events();
    let order_id = AggregateId::new("saga-unhosted-manager-order");

    // When
    let manager = SagaManagerProps::<NotingSaga>::new(|_components| NotingSaga)
        .observing::<OrderPlaced>(Subscription::stream(&order_id))
        .spawn(&system)
        .await;

    // Then
    assert!(
        system
            .process_system()
            .lookup(manager.manager().pid())
            .await
            .is_some(),
        "props built directly name the system at the spawn, and its process \
         system is where the manager runs"
    );

    append_order_placed(&journal, &order_id, 1, "SKU-UNHOSTED").await;
    assert_eq!(
        wait_for_noted(
            &journal,
            &saga_id,
            1,
            "the instance must journal onto the store the system was built on"
        )
        .await,
        vec!["SKU-UNHOSTED".to_owned()],
        "and the store the system was built on is where its instances journal"
    );
}
