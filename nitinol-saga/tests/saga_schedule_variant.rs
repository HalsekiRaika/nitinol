mod common;
use common::{outbox_kind_of, JsonCodec, OutboxKind};

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use nitinol_eventsource::{system::EventSourceSystem, Event, SequenceCursor};
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::{AggregateId, AppendingEvent, EventType, Family, LoadQuery, LoadedEvent, TypeName};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{Saga, SagaContext, SagaEffect, SagaId, SagaProps, Schedule};

#[derive(Clone, Debug, Serialize, Deserialize)]
struct OrderPlaced {
    sku: String,
}

impl Event for OrderPlaced {
    const EVENT_TYPE: EventType = EventType::new(Family::new("schedule"), TypeName::new("OrderPlaced"));
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ReservationRequested {
    sku: String,
}

impl Event for ReservationRequested {
    const EVENT_TYPE: EventType = EventType::new(Family::new("schedule"), TypeName::new("ReservationRequested"));
}

struct ScheduleSaga {
    handled: Arc<Notify>,
    at_a: jiff::Timestamp,
    at_b: jiff::Timestamp,
}

#[async_trait]
impl Saga for ScheduleSaga {
    type SubscribedEvent = OrderPlaced;
    type Event = ReservationRequested;
    type State = ();
    type Error = std::convert::Infallible;

    fn apply(&mut self, _event: Self::Event) {}

    async fn handle(
        &mut self,
        event: Self::SubscribedEvent,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        let effect = SagaEffect::persist(ReservationRequested { sku: event.sku })
            .with_schedules(vec![Schedule { at: self.at_a }, Schedule { at: self.at_b }]);
        self.handled.notify_one();
        Ok(effect)
    }
}

async fn append_order_placed(
    store: &Arc<dyn EventStore>,
    agg_id: &AggregateId,
    sequence: u64,
    sku: &str,
) {
    let payload = serde_json::to_vec(&OrderPlaced {
        sku: sku.to_owned(),
    })
    .map(Bytes::from)
    .expect("encode OrderPlaced must succeed");
    store
        .append(
            agg_id.as_str(),
            vec![AppendingEvent {
                sequence,
                event_type: OrderPlaced::EVENT_TYPE,
                payload,
                occurred_at: jiff::Timestamp::now(),
            }],
        )
        .await
        .expect("append OrderPlaced must succeed");
}

async fn load_saga_events(store: &Arc<dyn EventStore>, saga_id: &SagaId) -> Vec<LoadedEvent> {
    store
        .load(LoadQuery::by_stream(saga_id))
        .await
        .expect("load saga stream must succeed")
        .try_collect()
        .await
        .expect("collect saga events must succeed")
}

#[tokio::test]
async fn persist_with_schedules_appends_scheduled_markers_in_same_batch_and_takes_no_other_action()
{
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let order_id = AggregateId::new("schedule-order");
    append_order_placed(&upstream_store, &order_id, 1, "S-1").await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new("schedule-saga-1");
    let handled = Arc::new(Notify::new());

    let at_a = jiff::Timestamp::from_second(1_800_000_000)
        .expect("constructing a valid jiff::Timestamp must succeed");
    let at_b = jiff::Timestamp::from_second(1_800_000_060)
        .expect("constructing a valid jiff::Timestamp must succeed");

    let handled_clone = Arc::clone(&handled);

    let routed = saga_id.clone();
    let route_fn = move |_event: &OrderPlaced| -> Option<SagaId> { Some(routed.clone()) };

    let _saga_proxy =
        SagaProps::<ScheduleSaga>::new(saga_id.clone(), Arc::clone(&saga_store), move || {
            ScheduleSaga {
                handled: Arc::clone(&handled_clone),
                at_a,
                at_b,
            }
        })
        .with_codec(system.codec::<ReservationRequested>())
        .with_subscription(
            Arc::clone(&upstream_store),
            system.codec::<OrderPlaced>(),
            SequenceCursor::Stream {
                key: order_id.as_str().to_owned(),
                after: 0,
            },
            route_fn,
        )
        .spawn(system.process_system())
        .await;

    tokio::time::timeout(Duration::from_secs(3), handled.notified())
        .await
        .expect("Saga::handle must run within 3 seconds");

    // Give any spurious post-batch actions a moment to fire.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let events = load_saga_events(&saga_store, &saga_id).await;

    let user_count = events
        .iter()
        .filter(|e| e.event_type == ReservationRequested::EVENT_TYPE)
        .count();
    let scheduled_count = events
        .iter()
        .filter(|e| matches!(outbox_kind_of(e), Some(OutboxKind::Scheduled(_))))
        .count();
    let tell_requested_count = events
        .iter()
        .filter(|e| matches!(outbox_kind_of(e), Some(OutboxKind::TellRequested(_))))
        .count();
    let tell_acked_count = events
        .iter()
        .filter(|e| matches!(outbox_kind_of(e), Some(OutboxKind::TellAcked(_))))
        .count();
    let tell_failed_count = events
        .iter()
        .filter(|e| matches!(outbox_kind_of(e), Some(OutboxKind::TellFailed(_))))
        .count();

    assert_eq!(
        user_count, 1,
        "exactly one ReservationRequested must be persisted"
    );
    assert_eq!(
        scheduled_count, 2,
        "two Schedules in the Persist branch must yield two `scheduled` outbox events"
    );
    assert_eq!(
        tell_requested_count, 0,
        "schedules must not produce TellRequested markers (no tell intent on the branch)"
    );
    assert_eq!(
        tell_acked_count, 0,
        "no TellAcked must be appended in this MVP — scheduler execution is reserved for Issue γ"
    );
    assert_eq!(
        tell_failed_count, 0,
        "no TellFailed must be appended in this MVP — scheduler execution is reserved for Issue γ"
    );

    let mut scheduled_seconds: Vec<i64> = events
        .iter()
        .filter(|e| matches!(outbox_kind_of(e), Some(OutboxKind::Scheduled(_))))
        .map(|e| match outbox_kind_of(e) {
            Some(OutboxKind::Scheduled(p)) => p.at_unix_seconds,
            _ => unreachable!("filtered to Scheduled"),
        })
        .collect();
    scheduled_seconds.sort_unstable();
    let mut expected_seconds = vec![at_a.as_second(), at_b.as_second()];
    expected_seconds.sort_unstable();
    assert_eq!(
        scheduled_seconds, expected_seconds,
        "each `scheduled` marker payload must prost-decode to its Schedule's \
         unix-second timestamp"
    );

    let mut atomic_batch: Vec<&LoadedEvent> = events
        .iter()
        .filter(|e| {
            e.event_type == ReservationRequested::EVENT_TYPE
                || matches!(outbox_kind_of(e), Some(OutboxKind::Scheduled(_)))
        })
        .collect();
    atomic_batch.sort_by_key(|e| e.sequence);

    assert_eq!(
        atomic_batch.len(),
        3,
        "atomic Persist batch must be exactly 3 events (1 user + 2 scheduled)"
    );
    let seqs: Vec<u64> = atomic_batch.iter().map(|e| e.sequence).collect();
    assert_eq!(
        seqs,
        vec![1, 2, 3],
        "the atomic batch must occupy consecutive sequences starting from 1; \
         schedule markers MUST share the same store::append call as the user event"
    );

    assert_eq!(
        events.len(),
        3,
        "the saga stream must contain exactly the atomic batch: nothing else may be appended \
         for a Persist+Schedule branch in this MVP"
    );
}
