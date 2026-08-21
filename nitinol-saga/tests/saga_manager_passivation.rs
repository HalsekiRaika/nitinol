//! Both ends of the manager's idle-timeout boundary.
//!
//! An instance that has been idle for the configured passivation window is
//! stopped, and a later event for the same correlation id spawns it again with
//! its state restored from its own event stream.  The three ways that can be
//! got wrong are all observable here:
//! - passivation never reaches the child `Props` → the instance stays resident
//!   and only one spawn is ever recorded;
//! - the registry keeps the stopped instance's proxy → the revisit is delivered
//!   into a dead process and `handle` never runs a second time;
//! - the revisit spawns a fresh instance without replay → the second `handle`
//!   observes the initial state instead of the restored one.
//!
//! Idleness is an instance-level signal only: the manager itself owns the one
//! subscription and the registry, so it must outlive a system-wide default idle
//! timeout that a quiet upstream would otherwise let expire.

#[path = "common/helpers.rs"]
mod common;
use common::JsonCodec;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use nitinol_eventsource::{system::EventSourceSystem, Event, SequenceCursor};
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::{AggregateId, AppendingEvent, EventType, Family, TypeName};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{Saga, SagaContext, SagaEffect, SagaId, SagaManagerProps};

/// The one instance every `Ping` correlates to.
const INSTANCE_ID: &str = "mgr-passivation-instance";

/// Short enough to keep the test quick, long enough that it cannot elapse
/// between the manager spawning the instance and delivering the event that
/// caused the spawn.
const PASSIVATE_AFTER: Duration = Duration::from_millis(500);

/// The system-wide default the manager must not inherit.  Short so the quiet
/// stretch the second test waits out stays cheap.
const SYSTEM_IDLE_DEFAULT: Duration = Duration::from_millis(300);

// Domain types

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Ping {
    nonce: u64,
}

impl Event for Ping {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("mgr.passivation"), TypeName::new("Ping"));
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Counted;

impl Event for Counted {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("mgr.passivation"), TypeName::new("Counted"));
}

// Saga under test

/// Counts the `Counted` events it has seen.  Because the count is rebuilt only
/// by `apply`, the value `handle` observes on a revisit is evidence of whether
/// the instance replayed its own stream.
struct CountingSaga {
    seen: u64,
    observed: Arc<Mutex<Vec<u64>>>,
    notify: Arc<Notify>,
}

#[async_trait]
impl Saga for CountingSaga {
    type SubscribedEvent = Ping;
    type Event = Counted;
    type ScheduledMessage = ();
    type Error = std::convert::Infallible;

    fn correlate(_event: &Self::SubscribedEvent) -> Option<SagaId> {
        Some(SagaId::new(INSTANCE_ID))
    }

    fn apply(&mut self, _event: Self::Event) {
        self.seen += 1;
    }

    async fn handle(
        &mut self,
        _event: Self::SubscribedEvent,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        self.observed
            .lock()
            .expect("observed mutex is never poisoned: no holder panics while the guard is alive")
            .push(self.seen);
        self.notify.notify_one();
        Ok(SagaEffect::persist(Counted))
    }
}

// Helpers

async fn append_ping(store: &Arc<dyn EventStore>, upstream_key: &AggregateId, sequence: u64) {
    let payload = serde_json::to_vec(&Ping { nonce: sequence })
        .map(Bytes::from)
        .expect("encode Ping must succeed");
    store
        .append(
            upstream_key.as_str(),
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

fn observed_snapshot(observed: &Arc<Mutex<Vec<u64>>>) -> Vec<u64> {
    observed
        .lock()
        .expect("observed mutex is never poisoned: no holder panics while the guard is alive")
        .clone()
}

/// `context` states what the awaited delivery proves, so a timeout reports the
/// broken contract rather than only the count it was waiting for.
async fn wait_for_observed(
    observed: &Arc<Mutex<Vec<u64>>>,
    notify: &Arc<Notify>,
    expected: usize,
    context: &str,
) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let notified = notify.notified();
            if observed_snapshot(observed).len() >= expected {
                return;
            }
            notified.await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "timed out waiting for {expected} handle() call(s) — {context} (got {:?})",
            observed_snapshot(observed)
        )
    });
}

// Tests

#[tokio::test]
async fn idle_instance_is_passivated_and_restored_by_replay_on_revisit() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps)
        .with_codec::<JsonCodec>()
        .build();

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let upstream_key = AggregateId::new("mgr-passivation-pings");
    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());

    let observed: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::new()));
    let notify = Arc::new(Notify::new());
    let instances_spawned = Arc::new(AtomicUsize::new(0));

    let observed_for_producer = Arc::clone(&observed);
    let notify_for_producer = Arc::clone(&notify);
    let spawned_for_producer = Arc::clone(&instances_spawned);

    let _manager_proxy =
        SagaManagerProps::<CountingSaga>::new(Arc::clone(&saga_store), move || {
            spawned_for_producer.fetch_add(1, Ordering::SeqCst);
            CountingSaga {
                seen: 0,
                observed: Arc::clone(&observed_for_producer),
                notify: Arc::clone(&notify_for_producer),
            }
        })
        .with_codec(system.codec::<Counted>())
        .with_subscription(
            Arc::clone(&upstream_store),
            system.codec::<Ping>(),
            SequenceCursor::Stream {
                key: upstream_key.as_str().to_owned(),
                after: 0,
            },
        )
        .with_instance_passivation(PASSIVATE_AFTER)
        .spawn(system.process_system())
        .await;

    append_ping(&upstream_store, &upstream_key, 1).await;
    wait_for_observed(
        &observed,
        &notify,
        1,
        "the manager must deliver the first event to the instance it spawns",
    )
    .await;

    // The passivation window elapsing is the behaviour under test, so the test
    // waits it out rather than synchronising around it.  The multiplier keeps
    // the wait well clear of the runtime's idle-timer granularity.
    tokio::time::sleep(PASSIVATE_AFTER * 4).await;

    append_ping(&upstream_store, &upstream_key, 2).await;
    wait_for_observed(
        &observed,
        &notify,
        2,
        "a passivated instance must be spawned again for the next event of the \
         same correlation id",
    )
    .await;

    assert_eq!(
        instances_spawned.load(Ordering::SeqCst),
        2,
        "the instance must be passivated while idle and spawned again on the \
         revisit; a single spawn means with_instance_passivation never reached \
         the child process"
    );
    assert_eq!(
        observed_snapshot(&observed),
        vec![0, 1],
        "the revisited instance must recover the event it persisted before \
         passivation by replaying its own stream; observing 0 again would mean \
         it came back with initial state"
    );
}

/// Given a `ProcessSystem` carrying a default idle timeout,
/// when the manager sits through a quiet upstream for longer than that default,
/// then an event appended afterwards still reaches its instance — the manager
/// is not subject to the default the way its instances are.  A manager left on
/// `IdleTimeout::Inherit` stops here instead, taking the single subscription and
/// the registry with it and leaving nothing to revive the fan-out.
#[tokio::test]
async fn manager_outlives_a_system_default_idle_timeout_during_a_quiet_upstream() {
    let ps = ProcessSystem::builder()
        .with_default_idle_timeout(SYSTEM_IDLE_DEFAULT)
        .build()
        .await;
    let system = EventSourceSystem::builder(ps)
        .with_codec::<JsonCodec>()
        .build();

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let upstream_key = AggregateId::new("mgr-quiet-upstream-pings");
    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());

    let observed: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::new()));
    let notify = Arc::new(Notify::new());
    let observed_for_producer = Arc::clone(&observed);
    let notify_for_producer = Arc::clone(&notify);

    let _manager_proxy =
        SagaManagerProps::<CountingSaga>::new(Arc::clone(&saga_store), move || CountingSaga {
            seen: 0,
            observed: Arc::clone(&observed_for_producer),
            notify: Arc::clone(&notify_for_producer),
        })
        .with_codec(system.codec::<Counted>())
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

    // Nothing is appended until the default idle timeout has had time to fire:
    // the quiet upstream is the condition under test.
    tokio::time::sleep(SYSTEM_IDLE_DEFAULT * 4).await;

    append_ping(&upstream_store, &upstream_key, 1).await;
    wait_for_observed(
        &observed,
        &notify,
        1,
        "the manager must survive a quiet upstream that outlasts the system \
         default idle timeout and still deliver what arrives afterwards",
    )
    .await;
}
