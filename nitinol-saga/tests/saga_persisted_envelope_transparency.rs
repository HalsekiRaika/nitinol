//! A saga's own events are stored transparently: raw codec bytes under the
//! domain `EventType` the user declared, with no framework framing wrapped
//! around them.
//!
//! A saga's stream is mixed. Its own decisions share it with the framework's
//! records — `nitinol.saga.schedule`, `nitinol.saga.consumed`,
//! `nitinol.saga.lifecycle` — and the framework could have paid for that by
//! wrapping every row in one envelope declaration, so that the column named the
//! envelope and the payload carried what kind of row it really was. It does not:
//! the identity a row is stored under *is* the declaration, and the saga's own
//! events keep theirs.
//!
//! Two consequences, one per test:
//!
//! - what reaches the store for a persisted domain event is byte-for-byte what
//!   the event's own `Codec` produces, under the event's own `EventType`. A
//!   reader outside this framework can decode the saga's history knowing only
//!   the domain codec — and no row of a saga's own decisions lands inside the
//!   reserved `nitinol.*` namespace.
//! - replay classifies each row by the identity it is stored under, so a
//!   framework record is folded as framework bookkeeping and never reaches
//!   `Saga::apply`. Were it handed over, the saga's state would be rebuilt from
//!   rows it never decided — and a decode of those bytes through the domain
//!   codec would be a domain event the saga never emitted.
//!
//! Run with: `cargo test -p nitinol-saga --test saga_persisted_envelope_transparency`.

#[path = "common/helpers.rs"]
mod common;
use common::{encode_consumed_fact, JsonCodec, CONSUMED_MARKER, LIFECYCLE_MARKER, SCHEDULE_MARKER};

use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use nitinol_eventsource::{system::EventSourceSystem, Aggregate, Decider, Decision, Event};
use nitinol_persistence::provider::{CustomStore, PersistenceProvider};
use nitinol_persistence::store::{EventStore, InMemoryEventStore, InMemoryStore};
use nitinol_persistence::{
    is_within_reserved_namespace, AggregateId, AppendingEvent, EventType, Family, LoadQuery,
    LoadedEvent, TypeName,
};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{
    Saga, SagaContext, SagaEffect, SagaId, SagaManagerProps, SagaSubscribe, Subscription,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Triggered;

impl Event for Triggered {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("e2e.envelope.upstream"),
        TypeName::new("Triggered"),
    );
    type Codec = JsonCodec;
}

#[derive(Default)]
struct UpstreamAggregate;

impl Aggregate for UpstreamAggregate {
    type Event = Triggered;
    fn apply(&mut self, _event: Triggered) {}
}

struct Trigger;

impl Decider<Trigger> for UpstreamAggregate {
    type Output = ();
    type Rejection = Infallible;

    fn decide(&self, _cmd: Trigger) -> Decision<Triggered, (), Self::Rejection> {
        Decision::persist(vec![Triggered]).output(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct DomainEvt {
    note: String,
}

impl Event for DomainEvt {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("e2e.envelope"), TypeName::new("DomainEvt"));
    type Codec = JsonCodec;
}

/// Correlation rule of [`EnvelopeSaga`]: one instance owns the upstream stream
/// each test in this file wires it to.
const ENVELOPE_SAGA_ID: &str = "envelope-transparency-saga";

/// The whole alphabet this saga reacts to: one observed source, no deadline.
#[derive(SagaSubscribe)]
enum Observed {
    Triggered(Triggered),
}

struct EnvelopeSaga {
    applied: Arc<Mutex<Vec<DomainEvt>>>,
    persisted: Arc<Notify>,
    note: String,
}

#[async_trait]
impl Saga for EnvelopeSaga {
    type Subscribe = Observed;
    type Event = DomainEvt;
    type Error = Infallible;

    fn correlate(_input: &Self::Subscribe) -> Option<SagaId> {
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
        _input: Self::Subscribe,
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
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let system = EventSourceSystem::builder(ps)
        .with_persistence(PersistenceProvider::new(
            CustomStore::builder()
                .eventstore(Arc::clone(&store))
                .build(),
        ))
        .build();

    let upstream_id = AggregateId::new("envelope-encode-upstream");
    let upstream_proxy = system
        .spawn_aggregate::<UpstreamAggregate>(upstream_id.clone())
        .await;

    let saga_id = SagaId::new(ENVELOPE_SAGA_ID);
    let applied = Arc::new(Mutex::new(Vec::<DomainEvt>::new()));
    let persisted = Arc::new(Notify::new());

    let applied_for_saga = Arc::clone(&applied);
    let persisted_for_saga = Arc::clone(&persisted);

    let _host_proxy =
        SagaManagerProps::<EnvelopeSaga>::new(Arc::clone(&store), move |_components| {
            EnvelopeSaga {
                applied: Arc::clone(&applied_for_saga),
                persisted: Arc::clone(&persisted_for_saga),
                note: "persisted-by-handle".to_owned(),
            }
        })
        .observing::<Triggered>(Subscription::stream(&store, &upstream_id))
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
            .find(|e| e.event_type == DomainEvt::EVENT_TYPE)
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

    // The three identities a framework record can be stored under are the whole
    // of the reserved vocabulary a saga stream has; naming them individually
    // says which one a regression confused the domain event for.
    for framework_record in [SCHEDULE_MARKER, CONSUMED_MARKER, LIFECYCLE_MARKER] {
        assert_ne!(
            domain_event.event_type, framework_record,
            "a domain event must never be stored under a framework record's \
             identity: replay classifies by that identity alone, so the saga's \
             own decision would be folded as bookkeeping and never applied"
        );
    }
    assert!(
        !is_within_reserved_namespace(&domain_event.event_type.to_string()),
        "a saga's own event must stay outside the reserved `nitinol.*` \
         namespace; a row in there matching none of the three framework \
         declarations refuses the load outright"
    );
}

/// The upstream stream [`EnvelopeSaga`] is wired to in the replay test.
///
/// It is the stream the seeded consumed fact names, so the fact is one a real
/// incarnation could have written — and the one record it does hold sits below
/// that position, which is what makes the record inert: a host spawns an
/// instance when a record its correlation claims arrives, and this one wakes
/// the instance and is then dropped before `Saga::handle`.  Replay of the
/// seeded rows therefore stays the only thing that can reach `Saga::apply`.
const UNUSED_UPSTREAM: &str = "no-such-upstream";

/// The upstream sequence the inert boot record sits at, below the position the
/// seeded consumed fact holds for this source.
const BOOT_SEQUENCE: u64 = 7;

/// Given a saga stream holding one domain event and one framework record,
/// When a fresh incarnation replays it,
/// Then only the domain event reaches `Saga::apply`.
///
/// The framework record seeded here is a `nitinol.saga.consumed` position: a
/// row a real incarnation writes on this very stream, carrying prost bytes that
/// the saga's JSON codec would refuse. Classification is by the identity the
/// row is stored under, never by trying the domain codec and seeing what
/// happens — so a regression that handed it over would not be caught by a
/// decode failure, it would be caught here.
#[tokio::test]
async fn replay_applies_domain_event_and_never_applies_a_framework_record() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps)
        .with_persistence(PersistenceProvider::new(InMemoryStore::default()))
        .build();
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

    // The framework record is seeded the way the framework writes it: under the
    // consumed identity, with the fact carried by the payload's `oneof`.
    let framework_record = AppendingEvent {
        sequence: 2,
        event_type: CONSUMED_MARKER,
        payload: encode_consumed_fact(
            UNUSED_UPSTREAM,
            BOOT_SEQUENCE,
            &Triggered::EVENT_TYPE.to_string(),
        ),
        occurred_at: jiff::Timestamp::now(),
    };

    store
        .append(saga_id.as_str(), vec![domain_event, framework_record])
        .await
        .expect("seeding the saga stream must succeed");

    let applied = Arc::new(Mutex::new(Vec::<DomainEvt>::new()));
    let persisted = Arc::new(Notify::new());
    let applied_for_saga = Arc::clone(&applied);
    let persisted_for_saga = Arc::clone(&persisted);

    // The record that wakes the instance, at a position the seeded fact already
    // names: it reaches no `handle`, so what it adds is the instance's
    // existence and nothing else.
    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    upstream_store
        .append(
            UNUSED_UPSTREAM,
            vec![AppendingEvent {
                sequence: BOOT_SEQUENCE,
                event_type: Triggered::EVENT_TYPE,
                payload: serde_json::to_vec(&Triggered)
                    .map(Into::into)
                    .expect("encode Triggered must succeed"),
                occurred_at: jiff::Timestamp::now(),
            }],
        )
        .await
        .expect("appending the boot record must succeed");

    let _host_proxy =
        SagaManagerProps::<EnvelopeSaga>::new(Arc::clone(&store), move |_components| {
            EnvelopeSaga {
                applied: Arc::clone(&applied_for_saga),
                persisted: Arc::clone(&persisted_for_saga),
                note: "unused-on-replay".to_owned(),
            }
        })
        .observing::<Triggered>(Subscription::stream(&upstream_store, UNUSED_UPSTREAM))
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
        "replay must apply exactly the one domain event; the framework record \
         must be classified by the identity it is stored under and never \
         delivered to Saga::apply"
    );
    assert_eq!(
        applied[0],
        DomainEvt {
            note: "seeded-domain".to_owned()
        },
        "the domain event must be decoded via the codec and reconstructed exactly"
    );
}
