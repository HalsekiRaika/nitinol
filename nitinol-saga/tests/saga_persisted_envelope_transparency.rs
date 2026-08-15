#[path = "common/helpers.rs"]
mod common;
use common::{encode_outbox_tell_acked, JsonCodec};

use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use nitinol_eventsource::{
    system::EventSourceSystem, Aggregate, Context, Decider, Effect, Event, SequenceCursor,
};
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::{
    AggregateId, AppendingEvent, EventType, Family, LoadQuery, LoadedEvent, TypeName, Variant,
};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{Saga, SagaContext, SagaEffect, SagaId, SagaProps};

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Triggered;

impl Event for Triggered {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("e2e.envelope.upstream"),
        TypeName::new("Triggered"),
    );
}

#[derive(Default)]
struct UpstreamAggregate;

impl Aggregate for UpstreamAggregate {
    type Event = Triggered;
    fn apply(&mut self, _event: Triggered) {}
}

struct Trigger;

#[async_trait]
impl Decider<Trigger> for UpstreamAggregate {
    type Rejection = Infallible;

    async fn decide(
        &self,
        _cmd: Trigger,
        _ctx: &mut Context,
    ) -> Result<Effect<Triggered>, Self::Rejection> {
        Ok(Effect::persist(Triggered))
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct DomainEvt {
    note: String,
}

impl Event for DomainEvt {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("e2e.envelope"), TypeName::new("DomainEvt"));
}

/// Correlation rule of [`EnvelopeSaga`]: one instance owns the upstream stream
/// each test in this file wires it to.
const ENVELOPE_SAGA_ID: &str = "envelope-transparency-saga";

struct EnvelopeSaga {
    applied: Arc<Mutex<Vec<DomainEvt>>>,
    persisted: Arc<Notify>,
    note: String,
}

#[async_trait]
impl Saga for EnvelopeSaga {
    type SubscribedEvent = Triggered;
    type Event = DomainEvt;
    type ScheduledMessage = ();
    type Error = Infallible;

    fn correlate(_event: &Triggered) -> Option<SagaId> {
        Some(SagaId::new(ENVELOPE_SAGA_ID))
    }

    fn apply(&mut self, event: DomainEvt) {
        self.applied
            .lock()
            .expect("applied mutex is never poisoned: no holder panics while the guard is alive")
            .push(event);
    }

    async fn handle(
        &mut self,
        _event: Triggered,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<DomainEvt>, Self::Error> {
        let effect = SagaEffect::persist(DomainEvt {
            note: self.note.clone(),
        });
        self.persisted.notify_one();
        Ok(effect)
    }
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
async fn domain_event_is_persisted_as_raw_codec_payload_not_wrapped_by_envelope() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());

    let upstream_id = AggregateId::new("envelope-encode-upstream");
    let upstream_proxy = system
        .spawn_aggregate::<UpstreamAggregate>(upstream_id.clone(), Arc::clone(&store))
        .await;

    let saga_id = SagaId::new(ENVELOPE_SAGA_ID);
    let applied = Arc::new(Mutex::new(Vec::<DomainEvt>::new()));
    let persisted = Arc::new(Notify::new());

    let applied_for_saga = Arc::clone(&applied);
    let persisted_for_saga = Arc::clone(&persisted);

    let _saga_proxy =
        SagaProps::<EnvelopeSaga>::new(saga_id.clone(), Arc::clone(&store), move || EnvelopeSaga {
            applied: Arc::clone(&applied_for_saga),
            persisted: Arc::clone(&persisted_for_saga),
            note: "persisted-by-handle".to_owned(),
        })
        .with_codec(system.codec::<DomainEvt>())
        .with_subscription(
            Arc::clone(&store),
            system.codec::<Triggered>(),
            SequenceCursor::Stream {
                key: upstream_id.as_str().to_owned(),
                after: 0,
            },
        )
        .spawn(system.process_system())
        .await;

    upstream_proxy
        .ask(Trigger)
        .await
        .expect("ask(Trigger) must succeed");

    tokio::time::timeout(Duration::from_secs(3), persisted.notified())
        .await
        .expect("saga must handle the upstream event within 3 seconds");

    let deadline = Instant::now() + Duration::from_secs(3);
    let domain_event = loop {
        let events = load_saga_events(&store, &saga_id).await;
        if let Some(ev) = events
            .into_iter()
            .find(|e| e.event_type.type_key() == DomainEvt::EVENT_TYPE.type_key())
        {
            break ev;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for the persisted DomainEvt on the saga stream"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    };

    assert_eq!(
        domain_event.event_type,
        DomainEvt::EVENT_TYPE,
        "domain event must be stored under its own EventType; the transparent \
         envelope must not rewrite the wire event_type"
    );

    let decoded: DomainEvt = serde_json::from_slice(&domain_event.payload)
        .expect("stored payload must be the raw JSON codec output, not envelope-wrapped bytes");
    assert_eq!(
        decoded,
        DomainEvt {
            note: "persisted-by-handle".to_owned()
        },
        "the raw payload must round-trip through the domain codec unchanged"
    );
    let fresh = serde_json::to_vec(&DomainEvt {
        note: "persisted-by-handle".to_owned(),
    })
    .expect("encode must succeed");
    assert_eq!(
        domain_event.payload.as_ref(),
        fresh.as_slice(),
        "stored bytes must equal a fresh codec encode (no envelope proto framing)"
    );

    assert!(
        common::outbox_kind_of(&domain_event).is_none(),
        "a domain event must never appear as an outbox marker on the wire"
    );
}

#[tokio::test]
async fn replay_applies_domain_event_and_never_applies_outbox_marker() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());

    let saga_id = SagaId::new(ENVELOPE_SAGA_ID);

    let domain_payload = serde_json::to_vec(&DomainEvt {
        note: "seeded-domain".to_owned(),
    })
    .expect("encode");
    let domain_event = AppendingEvent {
        sequence: 1,
        event_type: DomainEvt::EVENT_TYPE,
        payload: domain_payload.into(),
        occurred_at: jiff::Timestamp::now(),
    };

    let outbox_type = EventType::with_variant(
        Family::new("nitinol.saga"),
        TypeName::new("outbox"),
        Variant::new("tell_acked"),
    );
    let outbox_event = AppendingEvent {
        sequence: 2,
        event_type: outbox_type,
        payload: encode_outbox_tell_acked(99),
        occurred_at: jiff::Timestamp::now(),
    };

    store
        .append(saga_id.as_str(), vec![domain_event, outbox_event])
        .await
        .expect("seeding the saga stream must succeed");

    let applied = Arc::new(Mutex::new(Vec::<DomainEvt>::new()));
    let persisted = Arc::new(Notify::new());
    let applied_for_saga = Arc::clone(&applied);
    let persisted_for_saga = Arc::clone(&persisted);

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());

    let _saga_proxy =
        SagaProps::<EnvelopeSaga>::new(saga_id.clone(), Arc::clone(&store), move || EnvelopeSaga {
            applied: Arc::clone(&applied_for_saga),
            persisted: Arc::clone(&persisted_for_saga),
            note: "unused-on-replay".to_owned(),
        })
        .with_codec(system.codec::<DomainEvt>())
        .with_subscription(
            Arc::clone(&upstream_store),
            system.codec::<Triggered>(),
            SequenceCursor::Stream {
                key: "no-such-upstream".to_owned(),
                after: 0,
            },
        )
        .spawn(system.process_system())
        .await;

    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if !applied
            .lock()
            .expect("applied mutex is never poisoned: no holder panics while the guard is alive")
            .is_empty()
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for replay to apply the seeded DomainEvt"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;

    let applied = applied
        .lock()
        .expect("applied mutex is never poisoned: no holder panics while the guard is alive");
    assert_eq!(
        applied.len(),
        1,
        "replay must apply exactly the one domain event; the outbox marker must \
         be classified by path prefix and never delivered to Saga::apply"
    );
    assert_eq!(
        applied[0],
        DomainEvt {
            note: "seeded-domain".to_owned()
        },
        "the domain event must be decoded via the codec and reconstructed exactly"
    );
}
