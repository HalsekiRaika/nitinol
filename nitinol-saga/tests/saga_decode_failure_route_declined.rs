//! `SagaProps::with_decode_failure_route` stays on the builder, and its
//! `None` answer still declines the failure.  (Contract `CT-05`.)
//!
//! Decode failures carry no typed event, so they cannot be correlated — the
//! decision of *which* subscriber owns a corrupt upstream event is routing, and
//! it stays an instance-level setting on `SagaProps` while correlation moves to
//! the `Saga` trait.  This file pins the branch that no existing test reaches:
//! a route function answering `None` records nothing.
//!
//! `saga_dead_letter_enqueue.rs` already covers the other branches — no route
//! configured (`upstream_message_decode_failure_enqueues_a_decode_failed_dead_letter`)
//! and `Some(other)` / `Some(self)` across two instances
//! (`decode_failure_route_fn_prevents_non_target_saga_from_recording_dead_letter`).
//!
//! Identifying power: an implementation that folds decode failures into
//! `Saga::correlate` loses the per-instance opt-out entirely, so the declining
//! saga below would record a `decode_failed` entry like any other subscriber.
//! The witness saga on the same upstream stream keeps the negative assertion
//! honest: it proves the corrupt event really was delivered to both
//! subscribers before the absence is checked.

#[path = "common/helpers.rs"]
mod common;
use common::JsonCodec;

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};

use nitinol_eventsource::{system::EventSourceSystem, Event, SequenceCursor};
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::{
    AggregateId, AppendingEvent, EventType, Family, LoadQuery, LoadedEvent, TypeName, Variant,
};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{Saga, SagaContext, SagaEffect, SagaId, SagaProps};

const UPSTREAM_KEY: &str = "decode-route-none-upstream";
const DECLINING_SAGA_ID: &str = "decode-route-none-declining";
const WITNESS_SAGA_ID: &str = "decode-route-none-witness";

const DEAD_LETTER_TYPE: EventType =
    EventType::new(Family::new("nitinol.saga"), TypeName::new("dead_letter"));

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Ping {
    /// Names the saga instance this ping is about.
    saga: String,
}

impl Event for Ping {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("saga.decode_route_none"), TypeName::new("Ping"));
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct SagaLog;

impl Event for SagaLog {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("saga.decode_route_none"),
        TypeName::new("SagaLog"),
    );
}

/// Both instances in this test are of this type: correlation is a property of
/// the type, while the decode-failure route differs per instance — which is
/// exactly why the latter stays on `SagaProps`.
struct DecodeRouteSaga;

#[async_trait]
impl Saga for DecodeRouteSaga {
    type SubscribedEvent = Ping;
    type Event = SagaLog;
    type ScheduledMessage = ();
    type Error = std::convert::Infallible;

    fn correlate(event: &Self::SubscribedEvent) -> Option<SagaId> {
        Some(SagaId::new(event.saga.clone()))
    }

    fn apply(&mut self, _event: Self::Event) {}

    async fn handle(
        &mut self,
        _event: Self::SubscribedEvent,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        Ok(SagaEffect::None)
    }
}

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
            e.event_type.type_key() == DEAD_LETTER_TYPE.type_key()
                && e.event_type.variant() == Some(Variant::new("decode_failed"))
        })
        .count()
}

async fn wait_for_decode_failed(store: &Arc<dyn EventStore>, saga_id: &SagaId) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if decode_failed_count(&load_saga_events(store, saga_id).await) >= 1 {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for the witness saga's `decode_failed` dead \
             letter; without it the absence assertion for the declining saga \
             would be vacuous"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Given two sagas subscribed to one upstream stream carrying a corrupt event,
/// when one of them routes decode failures to `None`,
/// then that saga records no `decode_failed` dead letter while the other, which
/// configured no route, records one.
#[tokio::test]
async fn decode_failure_route_returning_none_records_no_dead_letter() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    append_corrupt_ping(&upstream_store, 1).await;

    let cursor = SequenceCursor::Stream {
        key: UPSTREAM_KEY.to_owned(),
        after: 0,
    };

    let declining_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let declining_id = SagaId::new(DECLINING_SAGA_ID);
    let _declining = SagaProps::<DecodeRouteSaga>::new(
        declining_id.clone(),
        Arc::clone(&declining_store),
        || DecodeRouteSaga,
    )
    .with_codec(system.codec::<SagaLog>())
    .with_subscription(
        Arc::clone(&upstream_store),
        system.codec::<Ping>(),
        cursor.clone(),
    )
    .with_decode_failure_route(|_: &AggregateId, _: u64| -> Option<SagaId> { None })
    .spawn(system.process_system())
    .await;

    // Witness: no decode-failure route, so it records the failure.  Its entry
    // is the proof that the corrupt event reached the subscribers at all.
    let witness_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let witness_id = SagaId::new(WITNESS_SAGA_ID);
    let _witness =
        SagaProps::<DecodeRouteSaga>::new(witness_id.clone(), Arc::clone(&witness_store), || {
            DecodeRouteSaga
        })
        .with_codec(system.codec::<SagaLog>())
        .with_subscription(Arc::clone(&upstream_store), system.codec::<Ping>(), cursor)
        .spawn(system.process_system())
        .await;

    wait_for_decode_failed(&witness_store, &witness_id).await;

    // The declining saga runs its own poller; give it the same opportunity.
    tokio::time::sleep(Duration::from_millis(500)).await;

    assert_eq!(
        decode_failed_count(&load_saga_events(&declining_store, &declining_id).await),
        0,
        "a saga whose decode-failure route answers `None` declines the failure; \
         folding decode failures into `Saga::correlate` would remove this \
         per-instance opt-out and record an entry here"
    );
}
