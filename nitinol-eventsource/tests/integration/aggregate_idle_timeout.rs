//! An aggregate's activation stops after the idle timeout its props name, and
//! the next dispatch replays it back; a projector under the system default
//! idle timeout still projects what is appended after a quiet journal.
//!
//! ```gherkin
//!   Scenario: an activation passivates on the props' idle timeout and is replayed back
//!     Given an aggregate spawned with `with_idle_timeout(After(d))` on a system
//!           with no default idle timeout, that has decided one command
//!     When nothing is sent to it for longer than d
//!     And the same reference is asked again
//!     Then a second activation answers, deciding against the replayed state
//!     And that second activation stops after d too
//!
//!   Scenario: Persistent overrides the system default
//!     Given a system with a short default idle timeout
//!     And an aggregate spawned with `with_idle_timeout(Persistent)`
//!     When nothing is sent to it for longer than the default
//!     Then a plain ask is answered by the first activation
//!
//!   Scenario: props that name no idle timeout inherit the system default
//!     Given a system with a short default idle timeout
//!     And an aggregate spawned from props that name no idle timeout, or from
//!         an entry point that takes no props
//!     Then the activation stops after the default
//!
//!   Scenario: a projector projects what arrives after the system default
//!     Given a system with a short default idle timeout
//!     And a projector that has caught up
//!     When the journal stays quiet for longer than the default
//!     And one event is appended afterwards through the system's journal
//!     Then the projector projects it
//! ```
//!
//! How many activations an aggregate has had is counted where each one starts:
//! every activation builds its state from `Default` once, and nothing else in
//! the crate does. Each scenario names an aggregate type of its own, so the
//! counters do not leak between tests running in one binary.
//!
//! A dispatch to an activation that has already stopped is a transient
//! `AskError::Send`, and a reference re-resolves only on the dispatch
//! after it. Where the activation is expected to have stopped, the ask is made
//! through `ask_with_retry`, which absorbs that one transient failure; where it
//! is expected to still be running, a plain `ask` is used, so a stopped one
//! fails the test.
//!
//! The tests run on paused time: the idle timers, the quiet periods and the
//! probe bounds are all virtual, so a quiet period is exactly as long as it is
//! written and a loaded machine cannot stretch the gap between two dispatches
//! past an idle timeout.
//!
//! Run with:
//! `cargo test -p nitinol-eventsource --test integration aggregate_idle_timeout::`.

use std::future::Future;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use tokio::sync::Notify;

use nitinol_eventsource::codec::Codec;
use nitinol_eventsource::system::EventSourceSystem;
use nitinol_eventsource::{
    Aggregate, AggregateProps, AggregateProxy, Decider, Decision, Event, IdleTimeout, Never,
    ProjectionContext, ProjectionId, Projector, ProjectorProps, RetryPolicy,
};
use nitinol_persistence::provider::PersistenceProvider;
use nitinol_persistence::store::{InMemoryStore, Journal};
use nitinol_persistence::{AggregateId, AppendingEvent, EventType, Family, TypeName};
use nitinol_runtime::ProcessSystem;

/// The idle timeout under test, whether named on the props or as the system
/// default.
const IDLE: Duration = Duration::from_millis(50);

/// A quiet period long enough that an activation subject to [`IDLE`] has
/// stopped by its end.
const QUIET: Duration = Duration::from_millis(200);

/// How long, in virtual time, any single wait below may take before it is a
/// failure.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

// Fixtures: the event

/// Pass-through codec: `Incremented` carries no payload, so the format only has
/// to exist.
struct EventCodec;

impl Codec<Incremented> for EventCodec {
    type Error = std::convert::Infallible;

    fn encode(_event: &Incremented) -> Result<Bytes, Self::Error> {
        Ok(Bytes::new())
    }

    fn decode(_payload: &[u8]) -> Result<Incremented, Self::Error> {
        Ok(Incremented)
    }
}

#[derive(Clone, Debug, PartialEq)]
struct Incremented;

impl Event for Incremented {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("aggregate.idle.timeout"),
        TypeName::new("Incremented"),
    );

    type Codec = EventCodec;
}

// Fixtures: the aggregate

/// Where one aggregate type counts the activations it has had.
trait Activations: Send + Sync + 'static {
    fn started() -> &'static AtomicUsize;
}

/// Declare a marker type whose `Counter` counts its activations apart from
/// every other marker's.
macro_rules! activation_counter {
    ($marker:ident) => {
        struct $marker;

        impl Activations for $marker {
            fn started() -> &'static AtomicUsize {
                static STARTED: AtomicUsize = AtomicUsize::new(0);
                &STARTED
            }
        }
    };
}

activation_counter!(UnhostedAfter);
activation_counter!(HostedAfter);
activation_counter!(PersistentOverDefault);
activation_counter!(InheritedDefault);
activation_counter!(ContingentDefault);

/// Counts the events it has applied; the marker counts its activations.
struct Counter<K: Activations> {
    value: u64,
    _activations: PhantomData<fn() -> K>,
}

impl<K: Activations> Default for Counter<K> {
    fn default() -> Self {
        K::started().fetch_add(1, Ordering::SeqCst);
        Self {
            value: 0,
            _activations: PhantomData,
        }
    }
}

impl<K: Activations> Aggregate for Counter<K> {
    type Event = Incremented;
    type Recover = Never;

    fn apply(&mut self, _event: Incremented) {
        self.value += 1;
    }
}

/// Answers the counter's new value, so a decision taken against a replayed
/// stream answers one more than the events already in it.
#[derive(Clone)]
struct Increment;

impl<K: Activations> Decider<Increment> for Counter<K> {
    type Output = u64;
    type Rejection = std::convert::Infallible;

    fn decide(&self, _cmd: Increment) -> Decision<Incremented, u64, Self::Rejection> {
        Decision::persist(vec![Incremented]).output(self.value + 1)
    }
}

fn activations<K: Activations>() -> usize {
    K::started().load(Ordering::SeqCst)
}

// Helpers

/// A system distributing a store of its own, over `process_system`.
fn system_on(process_system: ProcessSystem) -> EventSourceSystem<InMemoryStore> {
    EventSourceSystem::builder(process_system)
        .with_persistence(PersistenceProvider::new(InMemoryStore::default()))
        .build()
}

/// A process system with no default idle timeout.
async fn without_default() -> ProcessSystem {
    ProcessSystem::new().await
}

/// A process system whose default idle timeout is [`IDLE`].
async fn with_default() -> ProcessSystem {
    ProcessSystem::builder()
        .with_default_idle_timeout(IDLE)
        .build()
        .await
}

/// `fut`, failing the test if it has not finished within [`PROBE_TIMEOUT`].
async fn within<F: Future>(what: &str, fut: F) -> F::Output {
    tokio::time::timeout(PROBE_TIMEOUT, fut)
        .await
        .unwrap_or_else(|_| panic!("{what} did not finish within {PROBE_TIMEOUT:?}"))
}

/// A policy whose waits are too short to matter, with room for the one
/// transient failure a dispatch to a stopped activation answers.
fn absorb_one_stopped_dispatch() -> RetryPolicy {
    RetryPolicy::new(3, Duration::from_millis(1), 2.0)
}

/// Ask `proxy` to increment, through a plain `ask`: the activation it reaches
/// is expected to be running.
async fn ask_running<K: Activations>(proxy: &AggregateProxy<Counter<K>>, why: &str) -> u64 {
    within("a plain ask", proxy.ask(Increment))
        .await
        .unwrap_or_else(|e| panic!("{why}, got {e:?}"))
}

/// Ask `proxy` to increment where the activation it last reached is expected
/// to have stopped.
async fn ask_after_stop<K: Activations>(proxy: &AggregateProxy<Counter<K>>, why: &str) -> u64 {
    within(
        "an ask after the activation stopped",
        proxy.ask_with_retry(Increment, &absorb_one_stopped_dispatch()),
    )
    .await
    .unwrap_or_else(|e| panic!("{why}, got {e:?}"))
}

// Scenario: an activation passivates on the props' idle timeout and is
// replayed back

/// The system has no default, so an activation that stops at all stops on the
/// timeout its props named; and the activation the reference re-resolves to
/// after that stops too, so the value outlives the activation it was first
/// spawned under.
#[tokio::test(start_paused = true)]
async fn an_activation_with_an_idle_timeout_passivates_and_the_next_ask_replays_it() {
    // Given
    let system = system_on(without_default().await);
    let proxy = AggregateProps::<Counter<UnhostedAfter>>::new(AggregateId::new(
        "aggregate-idle-timeout-unhosted-after",
    ))
    .with_idle_timeout(IdleTimeout::After(IDLE))
    .spawn(&system)
    .await;
    let first = ask_running(&proxy, "the freshly spawned activation must decide").await;
    assert_eq!(
        first, 1,
        "the first decision is taken against an empty stream"
    );

    // When: nothing is sent for longer than the props' idle timeout
    tokio::time::sleep(QUIET).await;
    let second = ask_after_stop(
        &proxy,
        "the reference must re-resolve the passivated aggregate and decide",
    )
    .await;

    // Then: a second activation decided, against the replayed first event
    assert_eq!(
        (activations::<UnhostedAfter>(), second),
        (2, 2),
        "the activation must have stopped after {IDLE:?} idle and a second one, replaying \
         the first command's event, must have decided — (activations, answer)"
    );

    // When: the second activation is left idle as well
    tokio::time::sleep(QUIET).await;
    let third = ask_after_stop(
        &proxy,
        "the reference must re-resolve the passivated aggregate again and decide",
    )
    .await;

    // Then: it stopped on the same timeout, and a third activation replayed both
    assert_eq!(
        (activations::<UnhostedAfter>(), third),
        (3, 3),
        "the activation a re-resolve starts must carry the props' idle timeout too; two \
         activations here means it was started without it — (activations, answer)"
    );
}

/// Props taken from the system take the idle timeout the same as props spawned
/// on one.
#[tokio::test(start_paused = true)]
async fn props_from_the_system_carry_their_idle_timeout_to_the_activation() {
    // Given
    let system = system_on(without_default().await);
    let proxy = system
        .aggregate_props::<Counter<HostedAfter>>(AggregateId::new(
            "aggregate-idle-timeout-hosted-after",
        ))
        .with_idle_timeout(IdleTimeout::After(IDLE))
        .spawn()
        .await;
    let first = ask_running(&proxy, "the freshly spawned activation must decide").await;
    assert_eq!(
        first, 1,
        "the first decision is taken against an empty stream"
    );

    // When
    tokio::time::sleep(QUIET).await;
    let second = ask_after_stop(
        &proxy,
        "the reference must re-resolve the passivated aggregate and decide",
    )
    .await;

    // Then
    assert_eq!(
        (activations::<HostedAfter>(), second),
        (2, 2),
        "props from the system must reach the activation with their idle timeout, so it \
         stops and a second one replays the first event — (activations, answer)"
    );
}

// Scenario: Persistent overrides the system default

#[tokio::test(start_paused = true)]
async fn persistent_props_keep_the_activation_through_the_system_default() {
    // Given
    let system = system_on(with_default().await);
    let proxy = AggregateProps::<Counter<PersistentOverDefault>>::new(AggregateId::new(
        "aggregate-idle-timeout-persistent",
    ))
    .with_idle_timeout(IdleTimeout::Persistent)
    .spawn(&system)
    .await;
    let first = ask_running(&proxy, "the freshly spawned activation must decide").await;
    assert_eq!(
        first, 1,
        "the first decision is taken against an empty stream"
    );

    // When: the system default would have stopped it several times over
    tokio::time::sleep(QUIET).await;

    // Then: the first activation is still there to answer a plain ask
    let second = ask_running(
        &proxy,
        "a Persistent activation must still be running after the system default idle timeout",
    )
    .await;
    assert_eq!(
        (activations::<PersistentOverDefault>(), second),
        (1, 2),
        "a Persistent activation must answer without being replayed — (activations, answer)"
    );
}

// Scenario: props that name no idle timeout inherit the system default

#[tokio::test(start_paused = true)]
async fn props_that_name_no_idle_timeout_stop_on_the_system_default() {
    // Given
    let system = system_on(with_default().await);
    let proxy = AggregateProps::<Counter<InheritedDefault>>::new(AggregateId::new(
        "aggregate-idle-timeout-inherit",
    ))
    .spawn(&system)
    .await;
    let first = ask_running(&proxy, "the freshly spawned activation must decide").await;
    assert_eq!(
        first, 1,
        "the first decision is taken against an empty stream"
    );

    // When
    tokio::time::sleep(QUIET).await;
    let second = ask_after_stop(
        &proxy,
        "the reference must re-resolve the passivated aggregate and decide",
    )
    .await;

    // Then
    assert_eq!(
        (activations::<InheritedDefault>(), second),
        (2, 2),
        "props that name no idle timeout must inherit the system default of {IDLE:?}, so \
         the activation stops and a second one replays — (activations, answer)"
    );
}

/// The contingent view of a system starts activations from no props at all,
/// and they inherit the system default as before.
#[tokio::test(start_paused = true)]
async fn an_aggregate_spawned_through_the_contingent_view_stops_on_the_system_default() {
    // Given
    let system = system_on(with_default().await);
    let proxy = system
        .to_contingent()
        .spawn_aggregate::<Counter<ContingentDefault>>(AggregateId::new(
            "aggregate-idle-timeout-contingent",
        ))
        .await;
    let first = ask_running(&proxy, "the freshly spawned activation must decide").await;
    assert_eq!(
        first, 1,
        "the first decision is taken against an empty stream"
    );

    // When
    tokio::time::sleep(QUIET).await;
    let second = ask_after_stop(
        &proxy,
        "the reference must re-resolve the passivated aggregate and decide",
    )
    .await;

    // Then
    assert_eq!(
        (activations::<ContingentDefault>(), second),
        (2, 2),
        "an activation started through the contingent view must inherit the system default \
         of {IDLE:?} — (activations, answer)"
    );
}

// Scenario: a projector projects what arrives after the system default

/// How many records a projection has applied, and a signal for each one.
#[derive(Clone, Default)]
struct ReadModel {
    applied: Arc<AtomicU64>,
    reached: Arc<Notify>,
}

impl ReadModel {
    /// Wait until the read model has counted `expected` records.
    async fn counted(&self, expected: u64, why: &str) {
        let watch = async {
            loop {
                let notified = self.reached.notified();
                if self.applied.load(Ordering::SeqCst) >= expected {
                    return;
                }
                notified.await;
            }
        };
        tokio::time::timeout(PROBE_TIMEOUT, watch)
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "{why}: the read model counted {} records in {PROBE_TIMEOUT:?}, \
                     wanted {expected}",
                    self.applied.load(Ordering::SeqCst)
                )
            });
    }
}

struct CountingProjector {
    model: ReadModel,
}

#[async_trait]
impl Projector<Incremented> for CountingProjector {
    type Error = std::convert::Infallible;

    async fn project(
        &mut self,
        _event: Incremented,
        _ctx: &mut ProjectionContext<'_, ()>,
    ) -> Result<(), Self::Error> {
        self.model.applied.fetch_add(1, Ordering::SeqCst);
        self.model.reached.notify_one();
        Ok(())
    }
}

/// Append the record numbered `sequence` to `stream`.
async fn append(journal: &Journal, stream: &AggregateId, sequence: u64) {
    journal
        .append(
            stream.as_str(),
            vec![AppendingEvent {
                sequence,
                event_type: Incremented::EVENT_TYPE,
                payload: Bytes::new(),
                occurred_at: jiff::Timestamp::now(),
            }],
        )
        .await
        .expect("appending the next sequence of a stream must succeed");
}

/// The poller hands the projector nothing on an empty read, so a quiet journal
/// is a projector receiving no messages, and a projector left on the system
/// default stops during the quiet period. The event appended afterwards goes
/// through the system's journal, whose append notification is what brings the
/// projector back to project it.
#[tokio::test(start_paused = true)]
async fn a_projector_projects_what_arrives_after_a_quiet_journal_under_the_system_default() {
    // Given: a projector that has caught up with the one record in its stream
    let system = system_on(with_default().await);
    let journal = system.event_store();
    let stream = AggregateId::new("aggregate-idle-timeout-projector");
    let model = ReadModel::default();
    let projector_model = model.clone();
    let _projector = ProjectorProps::new(
        ProjectionId::new("aggregate-idle-timeout-projector"),
        move || CountingProjector {
            model: projector_model.clone(),
        },
    )
    .with_event::<Incremented>()
    .catchup_from_aggregate(stream.clone())
    .with_poll_interval(Duration::from_millis(10))
    .spawn(&system)
    .await;
    append(&journal, &stream, 1).await;
    model
        .counted(1, "the projector must catch up with the first record")
        .await;

    // When: the journal stays quiet for longer than the system default, and
    // then one more record arrives
    tokio::time::sleep(QUIET).await;
    append(&journal, &stream, 2).await;

    // Then
    model
        .counted(
            2,
            "a projector must survive a quiet journal that outlasts the system default idle \
             timeout and project what arrives afterwards",
        )
        .await;
}
