//! `ProjectorProps::new` names no store, and the projector process holds the
//! journal its system distributes directly.
//!
//! The projector process owns its event-store reference inline; it catches up
//! by calling `store.load` directly instead of asking an `EventPersistor`
//! process.  The handle reaches it from the system the props are spawned on,
//! whose journal slot *is* a `Journal` — the props carry neither it nor the
//! offset store.
//!
//! Two complementary checks:
//!
//! 1. Compile-time type assertion — `ProjectorProps::new` receives the
//!    projection's identity and its producer, and nothing that names a store.
//! 2. Runtime integration — events pre-loaded into the journal the system was
//!    built on are observed by the projector during catchup.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use tokio::sync::Notify;

use nitinol_eventsource::system::EventSourceSystem;
use nitinol_eventsource::{
    codec::Codec, Event, ProjectionContext, ProjectionId, Projector, ProjectorProps,
};
use nitinol_persistence::provider::{CustomStore, PersistenceProvider};
use nitinol_persistence::store::{InMemoryEventStore, InMemoryOffsetStore};
use nitinol_persistence::{AggregateId, AppendingEvent, EventType, Family, TypeName};
use nitinol_runtime::ProcessSystem;

// Minimal Ping fixture

#[derive(Clone)]
struct PingEvent;

impl Event for PingEvent {
    const EVENT_TYPE: EventType = EventType::new(Family::new(""), TypeName::new("PingEvent"));

    type Codec = PingCodec;
}

struct PingCodec;

impl Codec<PingEvent> for PingCodec {
    type Error = std::convert::Infallible;
    fn encode(_event: &PingEvent) -> Result<Bytes, Self::Error> {
        Ok(Bytes::new())
    }
    fn decode(_payload: &[u8]) -> Result<PingEvent, Self::Error> {
        Ok(PingEvent)
    }
}

struct CountingProjector {
    count: Arc<AtomicUsize>,
    notify: Arc<Notify>,
}

#[async_trait]
impl Projector<PingEvent> for CountingProjector {
    type Error = std::convert::Infallible;

    async fn project(
        &mut self,
        _event: PingEvent,
        _ctx: &mut ProjectionContext<'_, ()>,
    ) -> Result<(), Self::Error> {
        self.count.fetch_add(1, Ordering::SeqCst);
        self.notify.notify_one();
        Ok(())
    }
}

// Compile-time type assertion

/// `ProjectorProps::new` MUST receive the projection's identity and its
/// producer, and nothing else — neither a journal nor the supplier that holds
/// one.  Not a `#[test]` — the compiler enforces the signature.
///
/// A regression to a receiving shape that also takes a store, a provider or an
/// offset store is a compile error here: the projector takes both of its stores
/// from the system it is spawned on.
#[allow(dead_code)]
fn _assert_sig_projector_props_new_receives_an_identity_and_a_producer() {
    let _: fn(ProjectionId, fn() -> CountingProjector) -> ProjectorProps<CountingProjector> =
        ProjectorProps::new;
}

// Runtime: catchup processes pre-loaded events via direct store

/// A projector spawned on a system catches up by reading events pre-loaded
/// into the journal that system was built on — confirming the projector calls
/// `store.load` directly (no proxy in between).
#[tokio::test]
async fn projector_catches_up_via_the_journal_it_was_wired_over() {
    // Given
    let system = EventSourceSystem::builder(ProcessSystem::new().await)
        .with_persistence(PersistenceProvider::new(
            CustomStore::builder()
                .eventstore(InMemoryEventStore::default())
                .projection_offsets(InMemoryOffsetStore::default())
                .build(),
        ))
        .build();
    let store = system.persistence().event_store();
    let agg_id = AggregateId::new("projector-direct-store");

    // Pre-populate the store with 3 events
    for seq in 1u64..=3 {
        store
            .append(
                agg_id.as_str(),
                vec![AppendingEvent {
                    sequence: seq,
                    event_type: EventType::new(Family::new(""), TypeName::new("PingEvent")),
                    payload: Bytes::new(),
                    occurred_at: jiff::Timestamp::now(),
                }],
            )
            .await
            .expect("pre-populate append must succeed");
    }

    let count = Arc::new(AtomicUsize::new(0));
    let notify = Arc::new(Notify::new());
    let count_c = Arc::clone(&count);
    let notify_c = Arc::clone(&notify);

    // When: spawn the projector on the system over the same journal (no
    // EventPersistor)
    let _proxy = ProjectorProps::new(ProjectionId::new("direct-store-projector"), move || {
        CountingProjector {
            count: Arc::clone(&count_c),
            notify: Arc::clone(&notify_c),
        }
    })
    .with_event::<PingEvent>()
    .catchup_from_aggregate(agg_id)
    .spawn(&system)
    .await;

    // Then: all 3 events are projected within the timeout
    tokio::time::timeout(Duration::from_millis(500), async {
        loop {
            let notified = notify.notified();
            if count.load(Ordering::SeqCst) >= 3 {
                return;
            }
            notified.await;
        }
    })
    .await
    .expect("all 3 events must be projected within 500ms via direct store");

    assert_eq!(
        count.load(Ordering::SeqCst),
        3,
        "project() must be called once per stored event (3 total)"
    );
}
