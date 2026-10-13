//! A saga reaching the system that hosts it.
//!
//! ```gherkin
//!   Scenario: an instance the host spawns is handed the hosting system
//!     Given a saga hosted through a system's own entry point
//!     When an instance it spawns asks the components for the hosting system
//!     Then it is handed one
//!     And telling an aggregate through it reaches the activation the host
//!         resolves, on the store the host was built on
//!
//!   Scenario: a saga built from props directly has no host
//!     Given a saga hosted from props built on a store rather than a system
//!     When its producer asks the components for the hosting system
//!     Then it is handed none
//! ```
//!
//! There is one hosted entry point and so one hosted clause.  The host builds
//! every instance from the one producer it was given, so there is no second
//! path a host handle could reach and no correlated instance it could miss.
//!
//! The producer is called with the capabilities the framework resolved, so this
//! is where a saga that needs a reference to tell gets one — rather than from a
//! clone of the system captured outside the spawn, which is what the host
//! handle replaces. No test here builds such a clone: whatever the saga reaches
//! it reached through the components it was handed.
//!
//! "The activation the host resolves" is observed through the store's
//! optimistic concurrency rule rather than through a process identifier. Two
//! activations of one stream each carry a sequence counter of their own, so the
//! one that fell behind has its append rejected: the third dispatch below is
//! what a host handle resolving through a registry of its own would cost. A
//! clause that only read the rows would pass such a handle, because a second
//! activation replays the stream and takes the next number quite happily.
//!
//! The last clause reads a handled count beside the absent host, because
//! "handed none" is otherwise indistinguishable from a producer that was never
//! called.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use nitinol_eventsource::codec::Codec;
use nitinol_eventsource::system::{EventSourceSystem, SystemHandle};
use nitinol_eventsource::{Aggregate, Decider, Decision, Event};
use nitinol_persistence::provider::{PersistenceProvider, Store};
use nitinol_persistence::store::{EventStore, InMemoryEventStore, InMemoryStore};
use nitinol_persistence::{AggregateId, AppendingEvent, EventType, Family, LoadQuery, TypeName};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{
    Saga, SagaComponents, SagaContext, SagaEffect, SagaId, SagaManagerProps, SagaSubscribe,
    SagaSystemExt, Subscription,
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
        Family::new("saga.components.host"),
        TypeName::new("OrderPlaced"),
    );
    type Codec = JsonCodec;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Noted;

impl Event for Noted {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("saga.components.host"), TypeName::new("Noted"));
    type Codec = JsonCodec;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Recorded {
    sku: String,
}

impl Event for Recorded {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("saga.components.host"),
        TypeName::new("Recorded"),
    );
    type Codec = JsonCodec;
}

/// A receiver whose decisions do not depend on what it has recorded, because
/// what a clause reads is the stream rather than the state.
#[derive(Default)]
struct Ledger;

impl Aggregate for Ledger {
    type Event = Recorded;

    fn apply(&mut self, _event: Recorded) {}
}

struct Record {
    sku: String,
}

impl Decider<Record> for Ledger {
    type Output = ();
    type Rejection = std::convert::Infallible;

    fn decide(&self, cmd: Record) -> Decision<Recorded, (), Self::Rejection> {
        Decision::persist(vec![Recorded { sku: cmd.sku }]).output(())
    }
}

/// The whole alphabet this saga reacts to: one observed source, no deadline.
#[derive(SagaSubscribe)]
enum Observed {
    Order(OrderPlaced),
}

/// The correlation every observed order falls under.
const SAGA_ID: &str = "saga-components-host";

/// The stream the saga records onto through the host it was handed.
const TARGET: &str = "saga-components-host-ledger";

/// What a clause reads back about the incarnation the framework built.
#[derive(Default)]
struct Report {
    host_present: AtomicBool,
    handled: AtomicUsize,
    notify: Notify,
}

/// A saga whose only dependency is the system that hosts it.
struct HostedSaga {
    /// The host the components answered with, if there was one.
    host: Option<SystemHandle>,
    report: Arc<Report>,
}

/// Build one incarnation, recording whether a host was handed over.
fn producer(report: &Arc<Report>) -> impl Fn(SagaComponents<HostedSaga>) -> HostedSaga {
    let report = Arc::clone(report);
    move |components| {
        let host = components.system().cloned();
        report.host_present.store(host.is_some(), Ordering::SeqCst);
        HostedSaga {
            host,
            report: Arc::clone(&report),
        }
    }
}

#[async_trait]
impl Saga for HostedSaga {
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
        if let Some(host) = &self.host {
            host.aggregate_proxy::<Ledger>(AggregateId::new(TARGET))
                .ask(Record { sku: event.sku })
                .await
                .expect("telling an aggregate through the host must succeed");
        }
        self.report.handled.fetch_add(1, Ordering::SeqCst);
        self.report.notify.notify_one();
        Ok(SagaEffect::None)
    }
}

// Helpers

const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

async fn append_order_placed(
    store: &Arc<dyn EventStore>,
    key: &AggregateId,
    sequence: u64,
    sku: &str,
) {
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

/// Stream sequences stored under `id`, in ascending order.
async fn stored_sequences(store: &dyn EventStore, id: &AggregateId) -> Vec<u64> {
    let loaded: Vec<_> = store
        .load(LoadQuery::by_stream(id))
        .await
        .expect("loading a stream of an in-memory store succeeds")
        .try_collect()
        .await
        .expect("collecting a stream of an in-memory store succeeds");
    loaded.iter().map(|event| event.sequence).collect()
}

/// The SKUs recorded on `id`, decoded with the codec `Recorded` names.
async fn recorded_skus(store: &dyn EventStore, id: &AggregateId) -> Vec<String> {
    let loaded: Vec<_> = store
        .load(LoadQuery::by_stream(id))
        .await
        .expect("loading a stream of an in-memory store succeeds")
        .try_collect()
        .await
        .expect("collecting a stream of an in-memory store succeeds");
    loaded
        .iter()
        .filter(|event| event.event_type == Recorded::EVENT_TYPE)
        .map(|event| {
            serde_json::from_slice::<Recorded>(&event.payload)
                .expect("the aggregate's event is encoded with the codec its type names")
                .sku
        })
        .collect()
}

async fn wait_for_handled(report: &Arc<Report>, want: usize, label: &str) {
    tokio::time::timeout(PROBE_TIMEOUT, async {
        loop {
            let notified = report.notify.notified();
            if report.handled.load(Ordering::SeqCst) >= want {
                return;
            }
            notified.await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "{label}: handle() was called {} times, wanted at least {want}",
            report.handled.load(Ordering::SeqCst)
        )
    });
}

// The system's own entry point

/// An instance a saga hosted from a system spawns is handed that system, and
/// what it tells through it reaches the host's activation on the host's store.
#[tokio::test]
async fn an_instance_the_host_spawns_reaches_the_hosting_systems_resources() {
    // Given: a system over a store this clause holds, with the target already
    // resolved and written to from outside
    let store = InMemoryStore::default();
    let system = EventSourceSystem::builder(ProcessSystem::new().await)
        .with_persistence(PersistenceProvider::new(store.clone()))
        .build();
    let target = AggregateId::new(TARGET);

    let from_outside = system.spawn_aggregate::<Ledger>(target.clone()).await;
    from_outside
        .ask(Record {
            sku: "from-outside".to_owned(),
        })
        .await
        .expect("the first dispatch must persist");

    let upstream: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let order_id = AggregateId::new("saga-components-host-order");
    let report = Arc::new(Report::default());

    // When: the saga is hosted through the system, with no system captured
    // outside the producer
    let _host = system
        .host::<HostedSaga>(producer(&report))
        .observing::<OrderPlaced>(Subscription::stream(&upstream, &order_id))
        .spawn()
        .await;

    append_order_placed(&upstream, &order_id, 1, "from-the-saga").await;
    wait_for_handled(&report, 1, "the host's instance must observe the order").await;

    // Then: the components answered with a host
    assert!(
        report.host_present.load(Ordering::SeqCst),
        "an instance hosted from a system is handed that system, so its \
         producer does not need a clone prepared outside the spawn"
    );

    // And: the third dispatch settles that the host resolves through the
    // registry the outer system resolves through
    from_outside
        .ask(Record {
            sku: "from-outside-again".to_owned(),
        })
        .await
        .expect("the outer reference must still be writing the same stream");

    assert_eq!(
        stored_sequences(&store.events(), &target).await,
        vec![1, 2, 3],
        "the host the saga was handed resolves through the hosting system's \
         aggregate registry; a registry of its own would have put a second \
         activation on this stream and cost one of these appends"
    );
    assert_eq!(
        recorded_skus(&store.events(), &target).await,
        vec![
            "from-outside".to_owned(),
            "from-the-saga".to_owned(),
            "from-outside-again".to_owned(),
        ],
        "and the host writes to the store the hosting system was built on, in \
         the order the three dispatches were made"
    );
}

// The entry point built directly

/// A saga hosted from props built on a store has no host, and says so.
///
/// This path needs no system to run on, so there is nothing truthful to answer
/// with. The handled count beside it is what makes the absence a statement
/// rather than a producer that never ran.
#[tokio::test]
async fn a_saga_built_from_props_directly_is_handed_no_host() {
    // Given
    let ps = ProcessSystem::new().await;
    let journal: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let upstream: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let order_id = AggregateId::new("saga-components-no-host-order");
    let report = Arc::new(Report::default());

    // When
    let _saga = SagaManagerProps::<HostedSaga>::new(Arc::clone(&journal), producer(&report))
        .observing::<OrderPlaced>(Subscription::stream(&upstream, &order_id))
        .spawn(&ps)
        .await;

    append_order_placed(&upstream, &order_id, 1, "without-a-host").await;
    wait_for_handled(&report, 1, "the instance must observe the order").await;

    // Then
    assert!(
        !report.host_present.load(Ordering::SeqCst),
        "props built on a store rather than on a system have no host to hand \
         over, and the components say so rather than naming one that does not \
         exist"
    );
}
