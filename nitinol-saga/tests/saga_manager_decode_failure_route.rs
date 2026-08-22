//! The manager's decode-failure attribution must actually reach a DLQ, and
//! must document the difference from `Saga::correlate` when it does not.
//!
//! A corrupt upstream record carries no typed event, so `Saga::correlate`
//! cannot name its owner.  `SagaManagerProps::with_decode_failure_route` closes
//! that gap: when it resolves a `SagaId`, the manager spawns/reuses that
//! instance and delivers `UpstreamMessage::DecodeFailed` to it, landing on the
//! same `SagaFailure::DecodeFailed` DLQ path `SagaProps` uses.  Without a
//! resolved owner (the function unset, or it returning `None`) there is
//! nowhere durable to record the failure, so it is skipped — the shared cursor
//! still advances past it, the same as a `Decoded` record `Saga::correlate`
//! claims for no instance.

#[path = "common/helpers.rs"]
mod common;
use common::JsonCodec;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use nitinol_eventsource::{system::EventSourceSystem, Event, SequenceCursor, SystemEvent};
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::{
    AggregateId, AppendingEvent, EventType, Family, LoadQuery, LoadedEvent, TypeName,
};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{
    DeadLetterEvent, Saga, SagaContext, SagaEffect, SagaFailure, SagaId, SagaManagerProps,
};

const UPSTREAM_KEY: &str = "mgr-decode-route-upstream";
const OWNER_SAGA_ID: &str = "mgr-decode-route-owner";

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Ping {
    /// Names the manager-spawned instance this ping correlates to.
    order: String,
}

impl Event for Ping {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("mgr.decode_route"), TypeName::new("Ping"));
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct SagaLog;

impl Event for SagaLog {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("mgr.decode_route"), TypeName::new("SagaLog"));
}

struct RouteSaga {
    seen: Arc<Mutex<Vec<String>>>,
    notify: Arc<Notify>,
}

#[async_trait]
impl Saga for RouteSaga {
    type SubscribedEvent = Ping;
    type Event = SagaLog;
    type ScheduledMessage = ();
    type Error = std::convert::Infallible;

    fn correlate(event: &Self::SubscribedEvent) -> Option<SagaId> {
        Some(SagaId::new(event.order.clone()))
    }

    fn apply(&mut self, _event: Self::Event) {}

    async fn handle(
        &mut self,
        event: Self::SubscribedEvent,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        self.seen
            .lock()
            .expect("seen mutex is never poisoned: no holder panics while the guard is alive")
            .push(event.order);
        self.notify.notify_one();
        Ok(SagaEffect::None)
    }
}

// Helpers

/// Append bytes carrying `Ping::EVENT_TYPE` that are not valid JSON for `Ping`:
/// the transform's type-key filter passes and `codec.decode` then fails,
/// producing `UpstreamMessage::DecodeFailed`.
async fn append_corrupt_ping(store: &Arc<dyn EventStore>, sequence: u64) {
    store
        .append(
            UPSTREAM_KEY,
            vec![AppendingEvent {
                sequence,
                event_type: Ping::EVENT_TYPE,
                payload: Bytes::from_static(b"NOT-VALID-JSON"),
                occurred_at: jiff::Timestamp::now(),
            }],
        )
        .await
        .expect("append corrupt Ping must succeed");
}

async fn append_ping(store: &Arc<dyn EventStore>, sequence: u64, order: &str) {
    let payload = serde_json::to_vec(&Ping {
        order: order.to_owned(),
    })
    .map(Bytes::from)
    .expect("encode Ping must succeed");
    store
        .append(
            UPSTREAM_KEY,
            vec![AppendingEvent {
                sequence,
                event_type: Ping::EVENT_TYPE,
                payload,
                occurred_at: jiff::Timestamp::now(),
            }],
        )
        .await
        .expect("append Ping must succeed");
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

fn decode_failed_count(events: &[LoadedEvent]) -> usize {
    events
        .iter()
        .filter(|e| {
            e.event_type.type_key() == <DeadLetterEvent as SystemEvent>::EVENT_TYPE.type_key()
                && matches!(
                    <DeadLetterEvent as SystemEvent>::decode(&e.payload),
                    Ok(DeadLetterEvent {
                        failure: SagaFailure::DecodeFailed { .. },
                        ..
                    })
                )
        })
        .count()
}

async fn wait_for_seen(seen: &Arc<Mutex<Vec<String>>>, notify: &Arc<Notify>, expected: usize) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let notified = notify.notified();
            if seen
                .lock()
                .expect("seen mutex is never poisoned: no holder panics while the guard is alive")
                .len()
                >= expected
            {
                return;
            }
            notified.await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!("timed out waiting for {expected} handle() calls");
    });
}

// Tests

/// Given a manager whose `with_decode_failure_route` names an owner for a
/// corrupt upstream record,
/// when the record is delivered,
/// then that owner's own stream carries exactly one `SagaFailure::DecodeFailed`
/// dead letter, and the shared cursor still advances so the next record still
/// reaches its instance.
#[tokio::test]
async fn decode_failure_route_names_an_owner_records_the_dead_letter_and_advances_the_cursor() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps)
        .with_codec::<JsonCodec>()
        .build();

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    append_corrupt_ping(&upstream_store, 1).await;
    append_ping(&upstream_store, 2, "AFTER-DECODE-FAILURE").await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let notify = Arc::new(Notify::new());

    let seen_for_producer = Arc::clone(&seen);
    let notify_for_producer = Arc::clone(&notify);

    let _manager_proxy =
        SagaManagerProps::<RouteSaga>::new(Arc::clone(&saga_store), move || RouteSaga {
            seen: Arc::clone(&seen_for_producer),
            notify: Arc::clone(&notify_for_producer),
        })
        .with_codec(system.codec::<SagaLog>())
        .with_subscription(
            Arc::clone(&upstream_store),
            system.codec::<Ping>(),
            SequenceCursor::Stream {
                key: UPSTREAM_KEY.to_owned(),
                after: 0,
            },
        )
        .with_decode_failure_route(|_: &AggregateId, _: u64| -> Option<SagaId> {
            Some(SagaId::new(OWNER_SAGA_ID))
        })
        .spawn(system.process_system())
        .await;

    // The valid record behind the corrupt one proves the shared cursor was not
    // held on the undecodable record.
    wait_for_seen(&seen, &notify, 1).await;
    assert_eq!(
        seen.lock()
            .expect("seen mutex is never poisoned: no holder panics while the guard is alive")
            .clone(),
        vec!["AFTER-DECODE-FAILURE".to_owned()],
        "the record after the corrupt one must still reach its instance"
    );

    let owner_id = SagaId::new(OWNER_SAGA_ID);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let recorded = loop {
        let count = decode_failed_count(&load_saga_events(&saga_store, &owner_id).await);
        if count >= 1 {
            break count;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for the resolved owner's DecodeFailed dead letter"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    assert_eq!(
        recorded, 1,
        "exactly one DecodeFailed dead letter must be recorded on the resolved \
         owner's own stream"
    );
}

/// Given a manager with no `with_decode_failure_route` configured,
/// when a corrupt upstream record arrives,
/// then no instance is attributed and no dead letter is recorded anywhere, but
/// the shared cursor still advances so the following record still reaches its
/// instance — the manager cannot invent an owner `Saga::correlate` has no way
/// to name, so this stays a skip, not a silent stall.
#[tokio::test]
async fn decode_failure_without_a_route_skips_and_advances_the_cursor() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps)
        .with_codec::<JsonCodec>()
        .build();

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    append_corrupt_ping(&upstream_store, 1).await;
    append_ping(&upstream_store, 2, "NO-ROUTE-AFTER-1").await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let notify = Arc::new(Notify::new());

    let seen_for_producer = Arc::clone(&seen);
    let notify_for_producer = Arc::clone(&notify);

    let _manager_proxy =
        SagaManagerProps::<RouteSaga>::new(Arc::clone(&saga_store), move || RouteSaga {
            seen: Arc::clone(&seen_for_producer),
            notify: Arc::clone(&notify_for_producer),
        })
        .with_codec(system.codec::<SagaLog>())
        .with_subscription(
            Arc::clone(&upstream_store),
            system.codec::<Ping>(),
            SequenceCursor::Stream {
                key: UPSTREAM_KEY.to_owned(),
                after: 0,
            },
        )
        .spawn(system.process_system())
        .await;

    wait_for_seen(&seen, &notify, 1).await;
    // Give the manager the same opportunity it had in the routed case to
    // (wrongly) attribute and record the corrupt record before asserting its
    // absence.
    tokio::time::sleep(Duration::from_millis(400)).await;

    assert_eq!(
        seen.lock()
            .expect("seen mutex is never poisoned: no holder panics while the guard is alive")
            .clone(),
        vec!["NO-ROUTE-AFTER-1".to_owned()],
        "the record after the corrupt one must still reach its instance; the \
         shared cursor must not stall on an unattributed decode failure"
    );

    let no_route_after_id = SagaId::new("NO-ROUTE-AFTER-1");
    assert_eq!(
        decode_failed_count(&load_saga_events(&saga_store, &no_route_after_id).await),
        0,
        "with no decode-failure route configured, the instance the following \
         record correlates to must not have a DecodeFailed dead letter — the \
         corrupt record belongs to no instance, not to whichever one happens \
         to be delivered next"
    );
}
