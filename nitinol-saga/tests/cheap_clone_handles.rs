//! Saga-side handles whose clones are another name for the one instance.
//!
//! ```gherkin
//!   Scenario: the pending deadline view is one per incarnation, filled by
//!             replay
//!     Given a saga instance that set a deadline whose schedule fact landed
//!     When the instance is passivated and a later record spawns it again
//!     Then the fresh incarnation's view holds that deadline, restored from the
//!          journal
//!
//!   Scenario: a host handle's clone reaches the host it was taken from
//!     Given a hosted saga whose delivery host is held on a refused delivery
//!     When an operator moves that partition on through a clone of the handle
//!     Then the running partition resumes, the handle it was cloned from
//!          observes the position as already finished with, and stopping the
//!          manager through either clone stops the one manager
//! ```
//!
//! The view is the thing a saga reads its own pending deadlines through, and it
//! is written only by the process that owns the stream — one per incarnation, so
//! a restarted incarnation starts from an empty one which replay then fills.
//! Both halves are observed on the one manager, which outlives the passivation:
//! the first probe reports an empty view, the instance is then passivated, and
//! the second probe is answered by a new incarnation that reports the deadline
//! back. The spawn count is what keeps that honest — an instance that was never
//! passivated would report the deadline from the view it had been keeping all
//! along.
//!
//! The delivery half is observed the way `saga_delivery_offset_advance_escape.rs`
//! observes the escape itself: an operation that wrote the store and left the
//! running partition reading from where it was would appear to work and change
//! nothing until the next restart. So the clause asks for the move through the
//! clone and then watches the delivery behind the hold be carried out — which is
//! the running partition's doing, not the store's.
//!
//! Run with: `cargo test -p nitinol-saga --test cheap_clone_handles`.

#[path = "common/helpers.rs"]
mod helpers;
use helpers::JsonCodec;

#[path = "common/delivery.rs"]
mod delivery;
use delivery::{
    append_decided, delivery_id, delivery_persistence, operations_of, settle,
    wait_until_operations, Ledger, RecordingEventStore, RecordingOffsetStore, SelectiveSaga,
    Triggered, PROBE_INTERVAL, PROBE_TIMEOUT, REFUSED_SUFFIX, TEST_POLL_INTERVAL,
};

use std::convert::Infallible;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;
use serde::{Deserialize, Serialize};

use nitinol::saga::SagaSubscribe;
use nitinol_eventsource::codec::Codec;
use nitinol_eventsource::system::EventSourceSystem;
use nitinol_eventsource::Event;
use nitinol_persistence::provider::PersistenceProvider;
use nitinol_persistence::store::{InMemoryEventStore, InMemoryStore, Journal};
use nitinol_persistence::{AggregateId, AppendingEvent, EventType, Family, TypeName};
use nitinol_runtime::ident::Pid;
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{
    Deadline, DeadlineMessage, PendingDeadline, Saga, SagaContext, SagaDeliveryError,
    SagaDeliveryProxy, SagaEffect, SagaHostProxy, SagaId, SagaManagerProps, SagaSystemExt,
    Subscription, TimerName,
};

// Fixtures: the alphabet

/// Asks the saga to report its pending deadlines, setting one first when the
/// payload names it.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Probe {
    set: Option<String>,
}

impl Event for Probe {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("cheap_clone.saga"), TypeName::new("Probe"));

    type Codec = JsonCodec;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Noted;

impl Event for Noted {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("cheap_clone.saga"), TypeName::new("Noted"));

    type Codec = JsonCodec;
}

#[derive(Clone, Debug, PartialEq)]
struct Reminder {
    label: String,
}

struct ReminderCodec;

impl Codec<Reminder> for ReminderCodec {
    type Error = Infallible;

    fn encode(value: &Reminder) -> Result<Bytes, Self::Error> {
        Ok(Bytes::from(value.label.clone().into_bytes()))
    }

    fn decode(payload: &[u8]) -> Result<Reminder, Self::Error> {
        Ok(Reminder {
            label: String::from_utf8_lossy(payload).into_owned(),
        })
    }
}

impl DeadlineMessage for Reminder {
    const TYPE_NAME: &'static str = "reminder";
    const CODEC_NAME: &'static str = "reminder-codec";
    type Codec = ReminderCodec;
}

// The deadline arm exists so the alphabet declares one — a saga whose producer
// asks for `Deadline<M>` has to name `M` here. Nothing in this file lets a
// firing happen, which `dead_code` cannot tell apart from a payload nothing
// fills.
#[allow(dead_code)]
#[derive(SagaSubscribe)]
enum Watched {
    Probe(Probe),
    #[subscribe(deadline)]
    Reminder(Reminder),
}

// Fixtures: the saga

/// The one instance every probe correlates to.
const SAGA_ID: &str = "cheap-clone-pending-view";

/// The stream the probes are appended to.
const UPSTREAM: &str = "cheap-clone-pending-view-upstream";

/// The timer the first probe sets.
const TIMER: &str = "far-off";

/// Far enough out that the deadline cannot fire inside this test: the subject is
/// what replay restores while the deadline is still pending, and a firing would
/// take it out of the view.
const FAR_OFF: Duration = Duration::from_secs(600);

/// Short enough to keep the test quick, long enough that it cannot elapse
/// between the manager spawning the instance and delivering the record that
/// caused the spawn.
const PASSIVATE_AFTER: Duration = Duration::from_millis(500);

/// What one probe reported: the names its view held.
type Reports = Arc<Mutex<Vec<Vec<String>>>>;

fn names_of(pending: &[PendingDeadline]) -> Vec<String> {
    let mut names: Vec<String> = pending
        .iter()
        .map(|entry| entry.name.as_str().to_owned())
        .collect();
    names.sort();
    names
}

struct ViewSaga {
    deadline: Deadline<Reminder>,
    reports: Reports,
}

#[async_trait]
impl Saga for ViewSaga {
    type Subscribe = Watched;
    type Event = Noted;
    type Error = Infallible;

    fn correlate(input: &Self::Subscribe) -> Option<SagaId> {
        match input {
            Watched::Probe(_) => Some(SagaId::new(SAGA_ID)),
            Watched::Reminder(_) => None,
        }
    }

    async fn handle(
        &mut self,
        input: Self::Subscribe,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        match input {
            Watched::Probe(probe) => {
                self.reports
                    .lock()
                    .expect("the reports lock guards a push and returns, so no holder panics")
                    .push(names_of(&self.deadline.pending()));
                match probe.set {
                    Some(name) => Ok(self.deadline.set(
                        TimerName::new(name.clone()),
                        FAR_OFF,
                        Reminder { label: name },
                    )),
                    None => Ok(SagaEffect::None),
                }
            }
            Watched::Reminder(_) => Ok(SagaEffect::None),
        }
    }
}

// Helpers

const BARRIER_TIMEOUT: Duration = Duration::from_secs(8);
const POLL_INTERVAL: Duration = Duration::from_millis(25);

async fn append_probe(store: &Journal, sequence: u64, set: Option<&str>) {
    let probe = Probe {
        set: set.map(str::to_owned),
    };
    store
        .append(
            UPSTREAM,
            vec![AppendingEvent {
                sequence,
                event_type: Probe::EVENT_TYPE,
                payload: serde_json::to_vec(&probe)
                    .map(Bytes::from)
                    .expect("encoding a fixture probe succeeds"),
                occurred_at: jiff::Timestamp::now(),
            }],
        )
        .await
        .expect("appending a fixture probe succeeds");
}

async fn wait_for_reports(reports: &Reports, expected: usize) -> Vec<Vec<String>> {
    let deadline = Instant::now() + BARRIER_TIMEOUT;
    loop {
        let current = reports
            .lock()
            .expect("the reports lock guards a read and returns, so no holder panics")
            .clone();
        if current.len() >= expected {
            return current;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {expected} probe report(s); saw {current:?}"
        );
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

// Scenario: the pending deadline view is one per incarnation, filled by replay

/// A passivated instance comes back with an empty view and fills it from its own
/// stream.
///
/// The second report is the clause: it is answered by an incarnation that was
/// built after the first one stopped, so the only way the deadline can be in its
/// view is replay — a view carried over from the incarnation that set it would
/// make this pass without the journal being read at all, which the spawn count
/// below rules out.
#[tokio::test]
async fn a_pending_view_is_restored_by_replay_after_passivation() {
    // Given: a saga whose instance passivates while idle, and a probe that sets
    // a deadline
    let system = EventSourceSystem::builder(ProcessSystem::new().await)
        .with_persistence(PersistenceProvider::new(InMemoryStore::default()))
        .build();
    // One journal: the probes the saga observes and the saga's own stream are
    // tenants of the store the system distributes, kept apart by key.
    let journal = system.persistence().event_store();

    let reports: Reports = Arc::new(Mutex::new(Vec::new()));
    let incarnations = Arc::new(AtomicUsize::new(0));

    let reports_for_producer = Arc::clone(&reports);
    let incarnations_for_producer = Arc::clone(&incarnations);
    let _manager = SagaManagerProps::<ViewSaga>::new(move |components| {
        incarnations_for_producer.fetch_add(1, Ordering::SeqCst);
        ViewSaga {
            deadline: components.deadline(),
            reports: Arc::clone(&reports_for_producer),
        }
    })
    .observing::<Probe>(Subscription::stream(UPSTREAM))
    .with_instance_passivation(PASSIVATE_AFTER)
    .spawn(&system)
    .await;

    append_probe(&journal, 1, Some(TIMER)).await;
    assert_eq!(
        wait_for_reports(&reports, 1).await,
        vec![Vec::<String>::new()],
        "the incarnation that sets the deadline starts from an empty view: \
         nothing is pending before its schedule fact lands"
    );

    // When: the instance is passivated, and a later probe spawns it again
    //
    // The passivation window elapsing is the behaviour the clause is built on,
    // so it is waited out rather than synchronised around. The multiplier keeps
    // the wait well clear of the runtime's idle-timer granularity.
    tokio::time::sleep(PASSIVATE_AFTER * 4).await;
    append_probe(&journal, 2, None).await;
    let observed = wait_for_reports(&reports, 2).await;

    // Then
    assert_eq!(
        incarnations.load(Ordering::SeqCst),
        2,
        "the instance must have been passivated and built again; a single \
         incarnation would answer the second probe from the view it never put \
         down"
    );
    assert_eq!(
        observed,
        vec![Vec::new(), vec![TIMER.to_owned()]],
        "the fresh incarnation's view is the journal's answer: the schedule fact \
         its predecessor landed is what puts the deadline back"
    );
}

// Scenario: a host handle's clone reaches the host it was taken from

/// The stream the delivery fixture saga's one declared source is bound to.
/// Nothing is ever appended to it.
const DELIVERY_UPSTREAM: &str = "cheap-clone-host-upstream";

fn assert_clone<T: Clone>() {}

/// Both halves of the handle one hosted declaration hands back are shareable by
/// cloning rather than by being wrapped in an `Arc`.
///
/// The delivery half has no runtime observation of its own here — it is reached
/// through [`SagaHostProxy`], which the clause below clones — so this is where
/// the type-level part of the contract is stated.
#[allow(dead_code)]
fn _saga_host_handles_are_clone() {
    assert_clone::<SagaDeliveryProxy<SelectiveSaga>>();
    assert_clone::<SagaHostProxy<SelectiveSaga>>();
}

/// A declared source nothing ever writes to, so what a clause reads off the
/// journal is the delivery host's doing and no one else's.
fn quiet_source() -> Subscription {
    Subscription::stream(&AggregateId::new(DELIVERY_UPSTREAM))
}

/// Wait until the delivery host has polled `store` at least `want` times.
///
/// Only the host's polls are counted: its subscription names the decision event
/// type as a prefix, while an aggregate replaying its stream names a single
/// stream and the manager's poller reads another store entirely.
async fn wait_until_delivery_polls(store: &RecordingEventStore, want: usize, label: &str) {
    let deadline = Instant::now() + PROBE_TIMEOUT;
    loop {
        let polls = store
            .queries()
            .into_iter()
            .filter(|query| query.event_type_prefix.is_some())
            .count();
        if polls >= want {
            return;
        }
        if Instant::now() >= deadline {
            panic!("{label}: saw {polls} polls of the journal, wanted at least {want}");
        }
        tokio::time::sleep(PROBE_INTERVAL).await;
    }
}

/// The registry is what says a process is gone: it is unregistered as the last
/// step of its own termination.
async fn wait_until_gone(ps: &ProcessSystem, pid: Pid, label: &str) {
    let deadline = Instant::now() + PROBE_TIMEOUT;
    loop {
        if ps.lookup(pid).await.is_none() {
            return;
        }
        if Instant::now() >= deadline {
            panic!("{label}: the process is still registered");
        }
        tokio::time::sleep(PROBE_INTERVAL).await;
    }
}

/// A clone of the handle one hosted declaration handed back names that same
/// manager and those same partitions.
///
/// The escape is the delivery clause: asked for through the clone, it has to
/// move the partition that is *running*, which is only observable as the
/// delivery behind the hold being carried out. The refusal that follows is the
/// other direction — the handle the clone came from reads the position the clone
/// moved as already finished with, which a second host reading a cursor of its
/// own would not report.
///
/// The manager clause is the same statement for the other half: stopping through
/// one clone takes down the process the other one names.
#[tokio::test]
async fn a_host_handle_clone_reaches_the_same_manager_and_partitions() {
    // Given: a hosted saga whose delivery host is held on a refused delivery,
    // with a reachable delivery behind it
    let journal = RecordingEventStore::new(InMemoryEventStore::default());
    let offsets = RecordingOffsetStore::new();
    let system = EventSourceSystem::builder(ProcessSystem::new().await)
        .with_persistence(delivery_persistence(&journal, &offsets))
        .build();

    let saga_id = SagaId::new("instance-1");
    let refused_target = format!("cheap-clone-host-target{REFUSED_SUFFIX}");
    let live_target = AggregateId::new("cheap-clone-host-ledger");

    let held = append_decided(&journal, &saga_id, 1, &refused_target, "op-refused").await;
    append_decided(&journal, &saga_id, 2, live_target.as_str(), "op-behind").await;

    let host = system
        .host::<SelectiveSaga>(|_components| SelectiveSaga)
        .observing::<Triggered>(quiet_source())
        .with_delivery_id(delivery_id())
        .with_delivery_poll_interval(TEST_POLL_INTERVAL)
        .spawn()
        .await;

    wait_until_delivery_polls(&journal, 3, "the host must reach the held record").await;
    settle().await;

    let ledger = system.aggregate_proxy::<Ledger>(live_target);
    assert!(
        operations_of(&ledger).await.is_empty(),
        "before the escape, the delivery behind the hold is unreachable"
    );

    // When: the operator moves the held partition on through a clone of the
    // handle, on the host that is still running
    let twin = host.clone();
    twin.delivery()
        .advance_offset(0, held)
        .await
        .expect("a clone of the handle must reach the partitions the declaration started");

    // Then: the running partition resumed, which is the half a write to the
    // store alone could not do
    assert_eq!(
        wait_until_operations(&ledger, 1, "the partition must resume after the escape").await,
        vec!["op-behind".to_owned()],
        "the clone names the running partitions of the host the original was \
         taken from: what lay behind the hold is delivered without the host \
         being restarted"
    );

    // And: the handle the clone came from reads the position the clone moved
    let refusal = host
        .delivery()
        .advance_offset(0, held)
        .await
        .expect_err("the position the clone moved to is already finished with");
    match refusal {
        SagaDeliveryError::NotForward {
            partition,
            to,
            reached,
        } => {
            assert_eq!(
                partition, 0,
                "the refusal must name the partition asked for"
            );
            assert_eq!(to, held, "the refusal must name the position asked for");
            assert!(
                reached >= held,
                "both handles read one partition's progress, so the original must \
                 see the clone's move as reached; it reported {reached}"
            );
        }
        other => panic!(
            "a move onto a position the clone already finished with must be \
             refused as not forward, got {other:?}"
        ),
    }

    // And: the manager half is one process too
    let manager_pid = host.manager().pid();
    twin.manager()
        .stop()
        .await
        .expect("stopping through the clone must succeed");
    wait_until_gone(
        system.process_system(),
        manager_pid,
        "a clone's `manager().stop()` must stop the manager the original names, \
         rather than a manager of its own",
    )
    .await;
}
