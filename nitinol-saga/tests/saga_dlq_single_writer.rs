//! The saga stream has exactly one writer, including for operator dispositions.
//!
//! `DeadLetterQueue::{mark_processed, evict}` no longer append to the saga's
//! own stream themselves.  They route the disposition through the
//! `SagaManager`, the single arbiter of that stream:
//!
//! | target state | who appends the marker             |
//! |--------------|------------------------------------|
//! | resident     | the `SagaProcess`, from its mailbox |
//! | dormant      | the manager itself                 |
//!
//! Everything here is observed through the public surface — the queue handed
//! out by [`SagaManagerProxy::dead_letter_queue`] and the bytes on the saga's
//! EventStore stream.  The wire representation of a dead letter and of a
//! disposition marker is unchanged and is pinned by
//! `saga_dead_letter_pull_api.rs`; what this file pins is *who writes it* and
//! what the operator gets back.

#[path = "common/helpers.rs"]
mod common;
use common::JsonCodec;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use nitinol_eventsource::system::EventSourceSystem;
use nitinol_eventsource::{appending_system_event, Event, SequenceCursor, SystemEvent};
use nitinol_persistence::error::{AppendError, LoadError};
use nitinol_persistence::store::{EventStore, EventStream, InMemoryEventStore};
use nitinol_persistence::{
    AggregateId, AppendOutcome, AppendingEvent, EventType, Family, LoadQuery, LoadedEvent,
    TypeName, Variant,
};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{
    DeadLetterEntry, DeadLetterEvent, DeadLetterQuery, DeadLetterQueue, DeadLetterQueueError, Saga,
    SagaContext, SagaEffect, SagaFailure, SagaId, SagaManagerProps, SagaManagerProxy,
    SourceContext,
};

// Wire identities observed here.

/// Type-level identity of a dead letter.
const DEAD_LETTER_TYPE: EventType =
    EventType::new(Family::new("nitinol.saga"), TypeName::new("dead_letter"));

/// Type-level identity of a disposition marker.
const DISPOSITION_TYPE: EventType = EventType::new(
    Family::new("nitinol.saga"),
    TypeName::new("dead_letter_disposition"),
);

/// The one correlation id every `Ping` routes to, so "the resident instance"
/// is a single well-known stream in every test below.
const RESIDENT_SAGA_ID: &str = "dlq-arbiter-resident";

/// The tag that makes `LedgerSaga::handle` fail, producing a `handle_failed`
/// dead letter on the saga's own stream.
const BOOM_TAG: &str = "boom";

/// How long a store-observed condition may take before the test reports the
/// contract it was waiting on.  Generous: it bounds a failure, not the
/// expected path.
const OBSERVE_TIMEOUT: Duration = Duration::from_secs(5);

// Domain types

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Ping {
    tag: String,
}

impl Event for Ping {
    const EVENT_TYPE: EventType = EventType::new(Family::new("dlq_arbiter"), TypeName::new("Ping"));
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Noted {
    tag: String,
}

impl Event for Noted {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("dlq_arbiter"), TypeName::new("Noted"));
}

#[derive(Debug)]
struct HandleBoom(String);

impl std::fmt::Display for HandleBoom {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ledger saga refused tag {}", self.0)
    }
}

impl std::error::Error for HandleBoom {}

// Saga under test

/// Persists one `Noted` per accepted `Ping` and fails on [`BOOM_TAG`], so a
/// test can both seed a dead letter and then prove the instance is still able
/// to append to its own stream.
struct LedgerSaga {
    handled: Arc<Mutex<Vec<String>>>,
    notify: Arc<Notify>,
}

#[async_trait]
impl Saga for LedgerSaga {
    type SubscribedEvent = Ping;
    type Event = Noted;
    type ScheduledMessage = ();
    type Error = HandleBoom;

    fn correlate(_event: &Self::SubscribedEvent) -> Option<SagaId> {
        Some(SagaId::new(RESIDENT_SAGA_ID))
    }

    fn apply(&mut self, _event: Self::Event) {}

    async fn handle(
        &mut self,
        event: Self::SubscribedEvent,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        if event.tag == BOOM_TAG {
            return Err(HandleBoom(event.tag));
        }
        self.handled
            .lock()
            .expect("handled mutex is never poisoned: no holder panics while the guard is alive")
            .push(event.tag.clone());
        self.notify.notify_one();
        Ok(SagaEffect::persist(Noted { tag: event.tag }))
    }
}

// A store that refuses exactly the disposition marker

/// Accepts every append except a disposition marker, so a test can reach the
/// state where settling a dead letter fails at the store while the saga itself
/// is perfectly able to keep writing.
#[derive(Default)]
struct RejectDispositionStore {
    inner: InMemoryEventStore,
}

fn is_disposition_marker(event: &AppendingEvent) -> bool {
    event
        .event_type
        .to_path()
        .is_within(&DISPOSITION_TYPE.to_path())
}

#[async_trait]
impl EventStore for RejectDispositionStore {
    async fn append(
        &self,
        key: &str,
        events: Vec<AppendingEvent>,
    ) -> Result<AppendOutcome, AppendError> {
        if events.iter().any(is_disposition_marker) {
            return Err(AppendError::Backend(
                "injected: the disposition marker cannot be written".into(),
            ));
        }
        self.inner.append(key, events).await
    }

    async fn load(&self, query: LoadQuery) -> Result<EventStream<'_>, LoadError> {
        self.inner.load(query).await
    }
}

// Harness

/// Everything a test needs to drive one manager: the stores it reads and
/// writes, what the saga handled, and how many instances were ever built.
struct Harness {
    manager: SagaManagerProxy<LedgerSaga>,
    saga_store: Arc<dyn EventStore>,
    upstream_store: Arc<dyn EventStore>,
    upstream_key: AggregateId,
    handled: Arc<Mutex<Vec<String>>>,
    notify: Arc<Notify>,
    instances_spawned: Arc<AtomicUsize>,
}

async fn spawn_harness(saga_store: Arc<dyn EventStore>, upstream_name: &str) -> Harness {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps)
        .with_codec::<JsonCodec>()
        .build();

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let upstream_key = AggregateId::new(upstream_name);

    let handled: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let notify = Arc::new(Notify::new());
    let instances_spawned = Arc::new(AtomicUsize::new(0));

    let handled_for_producer = Arc::clone(&handled);
    let notify_for_producer = Arc::clone(&notify);
    let spawned_for_producer = Arc::clone(&instances_spawned);

    let manager = SagaManagerProps::<LedgerSaga>::new(Arc::clone(&saga_store), move || {
        spawned_for_producer.fetch_add(1, Ordering::SeqCst);
        LedgerSaga {
            handled: Arc::clone(&handled_for_producer),
            notify: Arc::clone(&notify_for_producer),
        }
    })
    .with_codec(system.codec::<Noted>())
    .with_subscription(
        Arc::clone(&upstream_store),
        system.codec::<Ping>(),
        SequenceCursor::Stream {
            key: upstream_key.as_str().to_owned(),
            after: 0,
        },
    )
    .spawn(system.process_system())
    .await;

    Harness {
        manager,
        saga_store,
        upstream_store,
        upstream_key,
        handled,
        notify,
        instances_spawned,
    }
}

impl Harness {
    async fn append_ping(&self, sequence: u64, tag: &str) {
        let payload = serde_json::to_vec(&Ping {
            tag: tag.to_owned(),
        })
        .map(Bytes::from)
        .expect("encode Ping must succeed");
        self.upstream_store
            .append(
                self.upstream_key.as_str(),
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

    async fn stream_of(&self, saga_id: &SagaId) -> Vec<LoadedEvent> {
        load_stream(&self.saga_store, saga_id).await
    }

    fn spawned(&self) -> usize {
        self.instances_spawned.load(Ordering::SeqCst)
    }

    async fn wait_for_handled(&self, expected: usize, context: &str) {
        let handled = Arc::clone(&self.handled);
        let notify = Arc::clone(&self.notify);
        tokio::time::timeout(OBSERVE_TIMEOUT, async {
            loop {
                let notified = notify.notified();
                if handled_snapshot(&handled).len() >= expected {
                    return;
                }
                notified.await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "timed out waiting for {expected} handle() call(s) — {context} (got {:?})",
                handled_snapshot(&handled)
            )
        });
    }

    /// Poll the saga's own stream until `done` accepts it.  Used where the
    /// observable effect lands in the store after the handler returned, so
    /// there is no in-process signal to await.
    async fn wait_for_stream(
        &self,
        saga_id: &SagaId,
        context: &str,
        done: impl Fn(&[LoadedEvent]) -> bool,
    ) -> Vec<LoadedEvent> {
        let deadline = Instant::now() + OBSERVE_TIMEOUT;
        loop {
            let events = self.stream_of(saga_id).await;
            if done(&events) {
                return events;
            }
            if Instant::now() >= deadline {
                let types: Vec<String> = events.iter().map(|e| e.event_type.to_string()).collect();
                panic!("timed out waiting on the saga stream — {context} (event_types: {types:?})");
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
}

// Free helpers

fn handled_snapshot(handled: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
    handled
        .lock()
        .expect("handled mutex is never poisoned: no holder panics while the guard is alive")
        .clone()
}

async fn load_stream(store: &Arc<dyn EventStore>, saga_id: &SagaId) -> Vec<LoadedEvent> {
    store
        .load(LoadQuery::by_stream(saga_id))
        .await
        .expect("load saga stream must succeed")
        .try_collect()
        .await
        .expect("collect saga events must succeed")
}

fn of_type(events: &[LoadedEvent], event_type: EventType) -> Vec<&LoadedEvent> {
    events
        .iter()
        .filter(|e| e.event_type.type_key() == event_type.type_key())
        .collect()
}

fn count_variant(events: &[LoadedEvent], event_type: EventType, variant: &'static str) -> usize {
    events
        .iter()
        .filter(|e| {
            e.event_type.type_key() == event_type.type_key()
                && e.event_type.variant() == Some(Variant::new(variant))
        })
        .count()
}

fn sequences(entries: &[DeadLetterEntry]) -> Vec<u64> {
    entries.iter().map(|entry| entry.sequence).collect()
}

async fn list(queue: &DeadLetterQueue) -> Vec<DeadLetterEntry> {
    queue
        .list(DeadLetterQuery::default())
        .await
        .expect("list must succeed")
}

/// Seed one dead letter directly onto a saga's own stream, the way
/// `saga_dead_letter_pull_api.rs` does — used for the dormant case, where no
/// instance has ever run to produce one.
async fn seed_dead_letter(store: &Arc<dyn EventStore>, saga_id: &SagaId, sequence: u64) {
    let event = DeadLetterEvent {
        seq: sequence,
        saga_id: saga_id.clone(),
        failure: SagaFailure::HandleFailed {
            error: format!("seeded-failure-{sequence}"),
        },
        occurred_at_unix_millis: 1_700_000_000_000 + sequence as i64,
        source: SourceContext::without_upstream(),
    };
    store
        .append(
            saga_id.as_str(),
            vec![appending_system_event(
                sequence,
                &event,
                jiff::Timestamp::now(),
            )],
        )
        .await
        .expect("seeding a dead letter must succeed");
}

// Test 1 — the resident case: Rule "運用操作が常駐 saga を止めない", scenario 1

/// Given a resident saga whose stream already carries a dead letter,
/// When an operator marks it processed through the manager's queue,
/// Then the call answers with the outcome, the dead letter leaves `list`, its
/// original record stays in the store, and the saga goes on appending its own
/// events without a `persist_failed`.
///
/// The last clause is U-1 itself.  A queue that appended the marker directly
/// would place it at the sequence the resident instance still believes is its
/// own next one; the instance's following append then loses on
/// `unique(stream, sequence)`, retries into the same conflict, and stops on a
/// `persist_failed` that describes nothing but the collision.
#[tokio::test]
async fn resident_saga_keeps_appending_after_an_operator_settles_its_dead_letter() {
    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let harness = spawn_harness(Arc::clone(&saga_store), "dlq-arbiter-resident-pings").await;
    let saga_id = SagaId::new(RESIDENT_SAGA_ID);

    harness.append_ping(1, BOOM_TAG).await;
    harness
        .wait_for_stream(
            &saga_id,
            "handle() failing must enqueue a dead letter",
            |events| count_variant(events, DEAD_LETTER_TYPE, "handle_failed") == 1,
        )
        .await;

    let queue = harness.manager.dead_letter_queue(saga_id.clone());
    assert_eq!(
        sequences(&list(&queue).await),
        vec![1],
        "precondition: the operator sees the one outstanding dead letter"
    );

    queue
        .mark_processed(1)
        .await
        .expect("mark_processed must answer the operator with the outcome of the write");

    // Read back with no intervening wait: a request-reply settle has already
    // durably written when it returns, a fire-and-forget one has not.
    assert!(
        list(&queue).await.is_empty(),
        "the settled dead letter must be gone from `list` the moment `mark_processed` \
         returns — the operator's write is request-reply, not fire-and-forget"
    );

    let after_settle = harness.stream_of(&saga_id).await;
    assert_eq!(
        count_variant(&after_settle, DISPOSITION_TYPE, "processed"),
        1,
        "settling must append exactly one `processed` marker — no second writer may \
         add another"
    );
    assert_eq!(
        of_type(&after_settle, DEAD_LETTER_TYPE).len(),
        1,
        "settling is a logical delete: the original dead letter record must survive"
    );
    DeadLetterEvent::decode(&of_type(&after_settle, DEAD_LETTER_TYPE)[0].payload)
        .expect("the retained dead letter must still decode after being settled");

    // The saga must still own its stream's sequence.
    harness.append_ping(2, "after-settle").await;
    harness
        .wait_for_handled(
            1,
            "the saga must still receive upstream events after a settle",
        )
        .await;
    let after_business = harness
        .wait_for_stream(
            &saga_id,
            "the saga must be able to append its own event after the operator settled \
             a dead letter on the same stream",
            |events| {
                !of_type(events, Noted::EVENT_TYPE).is_empty()
                    || count_variant(events, DEAD_LETTER_TYPE, "persist_failed") > 0
            },
        )
        .await;

    // The rebuild count is the sharpest reading of U-1.  With two writers the
    // operator's marker takes the sequence the resident instance still believes
    // is its own next one; the instance's append conflicts, the `persist_failed`
    // it then tries to record conflicts for the very same reason and so is never
    // written, and supervision stops it.  The stream ends up looking healthy —
    // a replayed successor writes the event — and only the rebuild betrays that
    // a live saga was taken down by an operator's bookkeeping.
    assert_eq!(
        harness.spawned(),
        1,
        "the instance must never have been stopped and rebuilt: settling a dead \
         letter is not a saga failure"
    );
    assert_eq!(
        count_variant(&after_business, DEAD_LETTER_TYPE, "persist_failed"),
        0,
        "the saga must record no `persist_failed`: there is no sequence for the \
         operator's marker to have contended for"
    );
    assert_eq!(
        of_type(&after_business, Noted::EVENT_TYPE).len(),
        1,
        "the saga must persist the event it handled after the settle"
    );
}

// Test 2 — the dormant case: Rule "運用操作が常駐 saga を止めない", scenario 2

/// Given a saga with a dead letter and no resident instance,
/// When an operator evicts it through the manager's queue,
/// Then the manager writes the marker itself, the call answers with the
/// outcome, and the dead letter leaves `list`.
///
/// No instance may be spawned to do this: settling a dead letter is not a
/// reason to revive a saga that upstream traffic has not asked for.
#[tokio::test]
async fn dormant_saga_dead_letter_is_settled_by_the_manager_itself() {
    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let dormant_id = SagaId::new("dlq-arbiter-dormant");
    seed_dead_letter(&saga_store, &dormant_id, 1).await;

    // No upstream record is ever appended, so nothing correlates and no
    // instance is ever resident.
    let harness = spawn_harness(Arc::clone(&saga_store), "dlq-arbiter-dormant-pings").await;

    let queue = harness.manager.dead_letter_queue(dormant_id.clone());
    assert_eq!(
        sequences(&list(&queue).await),
        vec![1],
        "precondition: the dormant saga's dead letter is listed"
    );

    queue
        .evict(1)
        .await
        .expect("evict must answer the operator with the outcome of the write");

    assert!(
        list(&queue).await.is_empty(),
        "the evicted dead letter must be gone from `list` the moment `evict` returns"
    );

    let events = harness.stream_of(&dormant_id).await;
    assert_eq!(
        count_variant(&events, DISPOSITION_TYPE, "evicted"),
        1,
        "evicting a dormant saga's dead letter must append exactly one `evicted` marker"
    );
    assert_eq!(
        of_type(&events, DEAD_LETTER_TYPE).len(),
        1,
        "evict is a logical delete: the original dead letter record must survive"
    );
    assert_eq!(
        harness.spawned(),
        0,
        "no saga instance may be built to settle a dormant saga's dead letter — the \
         manager is the writer while nothing is resident"
    );
}

// Test 3 — the settle failed, and that is the operator's problem, not the saga's

/// Given a store that refuses the disposition marker,
/// When an operator marks a resident saga's dead letter processed,
/// Then the failure comes back to the operator as the queue's error and the
/// saga keeps running.
///
/// Carrying the append failure out as the message handler's `Err` instead
/// would put it through supervision, whose default for a saga instance is
/// `Stop` — an operator's failed bookkeeping write would take down a healthy
/// saga.  The rebuild count is what tells the two apart: a stopped instance
/// leaves the registry and the next upstream event builds a second one.
#[tokio::test]
async fn a_refused_disposition_is_reported_to_the_operator_without_stopping_the_saga() {
    let saga_store: Arc<dyn EventStore> = Arc::new(RejectDispositionStore::default());
    let harness = spawn_harness(Arc::clone(&saga_store), "dlq-arbiter-refused-pings").await;
    let saga_id = SagaId::new(RESIDENT_SAGA_ID);

    harness.append_ping(1, BOOM_TAG).await;
    harness
        .wait_for_stream(
            &saga_id,
            "handle() failing must enqueue a dead letter",
            |events| count_variant(events, DEAD_LETTER_TYPE, "handle_failed") == 1,
        )
        .await;

    let queue = harness.manager.dead_letter_queue(saga_id.clone());
    let outcome = queue.mark_processed(1).await;
    assert!(
        matches!(outcome, Err(DeadLetterQueueError::Append(_))),
        "a disposition the store refused must reach the operator as an append \
         failure, got {outcome:?}"
    );

    assert_eq!(
        count_variant(
            &harness.stream_of(&saga_id).await,
            DISPOSITION_TYPE,
            "processed"
        ),
        0,
        "a refused settle must leave no marker behind"
    );
    assert_eq!(
        sequences(&list(&queue).await),
        vec![1],
        "a refused settle must leave the dead letter outstanding for a retry"
    );

    harness.append_ping(2, "after-refused-settle").await;
    harness
        .wait_for_handled(
            1,
            "a refused settle must not stop the saga: the next upstream event must \
             still be handled",
        )
        .await;
    harness
        .wait_for_stream(
            &saga_id,
            "the saga must still append its own event after a refused settle",
            |events| !of_type(events, Noted::EVENT_TYPE).is_empty(),
        )
        .await;

    assert_eq!(
        harness.spawned(),
        1,
        "the instance must not have been stopped and rebuilt by the refused settle — \
         a second build means the append failure was raised through supervision \
         instead of answered to the operator"
    );
}

// Test 4 — routing the write through the arbiter did not drop the guard

/// Given a resident saga carrying one dead letter,
/// When an operator names a sequence that holds no dead letter,
/// Then the queue refuses it with `NotADeadLetter` and nothing is appended.
///
/// The guard belongs to the queue, which is what decides *what* is written;
/// the arbiter only decides *where*.  Moving the write behind the arbiter must
/// not lose it.
#[tokio::test]
async fn the_arbitrated_queue_still_refuses_a_sequence_that_is_not_a_dead_letter() {
    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let harness = spawn_harness(Arc::clone(&saga_store), "dlq-arbiter-guard-pings").await;
    let saga_id = SagaId::new(RESIDENT_SAGA_ID);

    harness.append_ping(1, BOOM_TAG).await;
    harness
        .wait_for_stream(
            &saga_id,
            "handle() failing must enqueue a dead letter",
            |events| count_variant(events, DEAD_LETTER_TYPE, "handle_failed") == 1,
        )
        .await;

    let queue = harness.manager.dead_letter_queue(saga_id.clone());
    let before = harness.stream_of(&saga_id).await.len();

    let marked = queue.mark_processed(99).await;
    assert!(
        matches!(marked, Err(DeadLetterQueueError::NotADeadLetter { .. })),
        "mark_processed on a sequence holding no dead letter must be refused, got {marked:?}"
    );
    let evicted = queue.evict(99).await;
    assert!(
        matches!(evicted, Err(DeadLetterQueueError::NotADeadLetter { .. })),
        "evict on a sequence holding no dead letter must be refused, got {evicted:?}"
    );

    assert_eq!(
        harness.stream_of(&saga_id).await.len(),
        before,
        "a refused disposition must append nothing at all"
    );

    // The guard rejects the wrong target, not every target.
    queue
        .mark_processed(1)
        .await
        .expect("the real dead letter must still be settleable through the arbiter");
    assert!(list(&queue).await.is_empty());
}
