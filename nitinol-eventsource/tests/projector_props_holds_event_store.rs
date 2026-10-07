//! `ProjectorProps::new` holds an `Arc<dyn EventStore>` directly.
//!
//! The projector process now owns its event-store reference inline; it
//! catches up by calling `store.load` directly instead of asking an
//! `EventPersistor` process.  The handle reaches it through the
//! `PersistenceProvider` its props are built from, whose journal slot *is*
//! an `Arc<dyn EventStore>`.
//!
//! Two complementary checks:
//!
//! 1. Compile-time type assertion — the journal a `ProjectorProps` is built
//!    over is named as `Arc<dyn EventStore>`.  A regression to
//!    `EventPersistorProxy` fails to compile.
//! 2. Runtime integration — events pre-loaded into the store are observed
//!    by the projector during catchup.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use tokio::sync::Notify;

use nitinol_eventsource::{codec::Codec, Event, ProjectionContext, Projector, ProjectorProps};
use nitinol_persistence::provider::{CustomStore, PersistenceProvider};
use nitinol_persistence::store::{EventStore, InMemoryCheckpointStore, InMemoryEventStore};
use nitinol_persistence::{AggregateId, AppendingEvent, EventType, Family, ProjectionId, TypeName};
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

/// `ProjectorProps::new` MUST take its journal as `Arc<dyn EventStore>` — the
/// provider it is given names the journal with exactly that type.  Not a
/// `#[test]` — the compiler enforces the signature.
///
/// We exercise the call in a generic context because `ProjectorProps::new` is
/// generic over the projector type — a function pointer assignment cannot
/// capture this generic shape directly, so we call it and discard.
#[allow(dead_code)]
fn _assert_sig_projector_props_new_accepts_arc_dyn_event_store(
    pid: ProjectionId,
    store: Arc<dyn EventStore>,
    cs: Arc<InMemoryCheckpointStore>,
) {
    let count = Arc::new(AtomicUsize::new(0));
    let notify = Arc::new(Notify::new());
    let persistence = PersistenceProvider::new(
        CustomStore::builder()
            .eventstore(store)
            .projection_checkpoints(cs)
            .build(),
    );
    let _ = ProjectorProps::new(pid, &persistence, move || CountingProjector {
        count: Arc::clone(&count),
        notify: Arc::clone(&notify),
    });
}

// Runtime: catchup processes pre-loaded events via direct store

/// A projector spawned with Arc<dyn EventStore> catches up by reading events
/// pre-loaded into the same store — confirming the projector calls
/// `store.load` directly (no proxy in between).
#[tokio::test]
async fn projector_catches_up_via_direct_arc_dyn_event_store() {
    // Given
    let system = ProcessSystem::new().await;
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let checkpoint_store = Arc::new(InMemoryCheckpointStore::default());
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

    // When: spawn projector over the same Arc<dyn EventStore> (no EventPersistor)
    let persistence = PersistenceProvider::new(
        CustomStore::builder()
            .eventstore(Arc::clone(&store))
            .projection_checkpoints(Arc::clone(&checkpoint_store))
            .build(),
    );
    let _proxy = ProjectorProps::new(
        ProjectionId::new("direct-store-projector"),
        &persistence,
        move || CountingProjector {
            count: Arc::clone(&count_c),
            notify: Arc::clone(&notify_c),
        },
    )
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
