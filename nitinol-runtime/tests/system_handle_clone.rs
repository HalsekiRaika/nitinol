//! `ProcessSystem` as a shared handle, and `ProcessSystemBuilder` as the single
//! place its configuration is fixed.
//!
//! Two contracts meet here.
//!
//! * **The system type is the handle.**  Cloning a `ProcessSystem` produces
//!   another name for the same instance rather than a second one: every clone
//!   observes the same process registry, the same dead-letter stream, and the
//!   same defaults.  That is what lets callers drop the outer
//!   `Arc<ProcessSystem>` — keeping the instance single is the library's job,
//!   and whether to clone is the caller's choice.
//! * **Configuration is fixed at build time.**  `with_default_*` belongs to
//!   `ProcessSystemBuilder`, and `build().await` performs the construction —
//!   including spawning the dead-letter infrastructure — handing back a system
//!   with no remaining way to be reconfigured.
//!
//! Every assertion below is written against what a caller can observe through
//! the public API: which process a lookup resolves, which stream a dead letter
//! reaches, whether a spawned process decays to the configured default.  Nothing
//! inspects the handle's representation — "cheap to clone" is a cost property,
//! not an observable one, so the tests pin the sharing it is supposed to deliver
//! instead.
//!
//! Two parts of the contract are deliberately *not* here.
//!
//! * That `with_default_*` is **not** callable on a built `ProcessSystem` cannot
//!   be expressed from a Cargo integration test — a method that does not exist
//!   fails to compile rather than fails an assertion, and this crate has no
//!   `trybuild` harness (the same note appears in `props_macro.rs` and
//!   `stream_props.rs`).  It belongs in a `compile_fail` doctest beside the
//!   builder, in the form already used on `ProcessSystem::spawn`.
//! * Equivalence with an outer `Arc<ProcessSystem>` is the conjunction of the
//!   sharing tests below, not a test of its own: wrapping a system in an `Arc`
//!   here to compare against would reintroduce exactly the pattern this change
//!   removes from the repository.

#[path = "common/helpers.rs"]
mod common;

use std::future::Future;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use nitinol_runtime::ident::ProcessName;
use nitinol_runtime::process::{ProcessProxy, SubscriberContext};
use nitinol_runtime::{BoxedMessage, IdleTimeout, ProcessSystem, Props, Subscriber};

use common::{test_props, tracked_state, wait_for_flag, Increment, TrackedProcess};

/// How long any state-based wait may run before the test gives up.
const WAIT_TIMEOUT: Duration = Duration::from_secs(5);

// Fixtures

/// Counts every message published to the stream it is subscribed to.
struct DeadLetterCounter {
    count: Arc<AtomicU32>,
}

impl Subscriber<BoxedMessage> for DeadLetterCounter {
    fn recv(
        &mut self,
        _msg: BoxedMessage,
        _ctx: &mut SubscriberContext<'_, BoxedMessage>,
    ) -> impl Future<Output = ()> + Send {
        let count = Arc::clone(&self.count);
        async move {
            count.fetch_add(1, Ordering::SeqCst);
        }
    }
}

/// Spawn a [`DeadLetterCounter`] on `system`, subscribe it to that system's
/// dead-letter stream, and return the counter it increments.
///
/// `Persistent` because one caller below builds a system whose default idle
/// timeout is deliberately shorter than the test that uses it; an `Inherit`
/// subscriber would be reaped before it could observe anything.
///
/// The subscribe is itself an assertion: registering on a stream that was never
/// spawned, or that has since stopped, fails to send.
async fn subscribe_dead_letter_counter(system: &ProcessSystem) -> Arc<AtomicU32> {
    let count = Arc::new(AtomicU32::new(0));
    let props = Props::subscriber({
        let count = Arc::clone(&count);
        move || DeadLetterCounter {
            count: Arc::clone(&count),
        }
    })
    .with_idle_timeout(IdleTimeout::Persistent);

    let proxy = system.spawn(props).await;
    system
        .dead_letter_stream()
        .subscribe(proxy)
        .await
        .expect("the dead-letter stream must be running and accepting subscribers");

    count
}

/// Spawn a process on `system`, wait for it to start, and stop it.
///
/// The returned proxy still addresses the pid that has just left the registry —
/// which is what makes a tell through it dead-letter.
async fn spawn_and_stop(system: &ProcessSystem) -> ProcessProxy<TrackedProcess> {
    let (started, stopped, counter) = tracked_state();
    let proxy = system
        .spawn(test_props(started.clone(), stopped.clone(), counter))
        .await;
    wait_for_flag(&started).await;
    proxy.stop().await.expect("stop must succeed");
    wait_for_flag(&stopped).await;
    proxy
}

/// Tell an already-stopped `proxy` until a dead letter reaches `count`, failing
/// on the deadline with `context`.
///
/// A stopped process closes its mailbox as the last step of its shutdown, so the
/// first tell after `on_stop` may still be accepted by a channel that has not
/// closed yet.  Retrying waits for the state under assertion rather than
/// guessing a sleep long enough to reach it.
async fn tell_until_dead_lettered(
    proxy: &ProcessProxy<TrackedProcess>,
    count: &AtomicU32,
    context: &str,
) {
    let deadline = Instant::now() + WAIT_TIMEOUT;
    while count.load(Ordering::SeqCst) == 0 {
        assert!(Instant::now() < deadline, "{context}");
        let _ = proxy.tell(Increment).await;
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

// Type-level contracts

fn assert_clone<T: Clone>() {}

/// The system type is itself the shared handle, so callers never need to wrap
/// one to share it.
///
/// That `with_default_*` lives on `ProcessSystemBuilder` — and that the chain
/// ends at `build()` — is pinned by `system_defaults_inherit.rs`, which owns the
/// defaults contract; it is not restated here.
#[allow(dead_code)]
fn _process_system_is_clone() {
    assert_clone::<ProcessSystem>();
}

// Sharing across clones

/// Clones resolve each other's processes: there is one registry behind them all.
///
/// Both directions are asserted because both are the same contract seen from
/// either end — an implementation that gave each clone its own registry would
/// leave a caller unable to find a process it can already talk to, which is the
/// state `lookup_returns_none_for_unknown_pid` pins for two genuinely separate
/// systems.
#[tokio::test]
async fn clone_and_original_share_one_process_registry() {
    // Given: a system and a clone of it
    let system = ProcessSystem::new().await;
    let handle = system.clone();

    // When: one named process is spawned through each
    let (started_a, stopped_a, counter_a) = tracked_state();
    let name = ProcessName::new("handle-shared-worker");
    let through_original = system
        .spawn(test_props(started_a.clone(), stopped_a, counter_a).with_name(name.clone()))
        .await;
    let pid_a = through_original.pid();
    wait_for_flag(&started_a).await;

    let (started_b, stopped_b, counter_b) = tracked_state();
    let through_clone = handle
        .spawn(test_props(started_b.clone(), stopped_b, counter_b))
        .await;
    let pid_b = through_clone.pid();
    wait_for_flag(&started_b).await;

    // Then: each side resolves what the other spawned
    assert!(
        handle.lookup(pid_a).await.is_some(),
        "the clone must resolve a process spawned through the original; a \
         per-clone registry would leave this lookup empty"
    );
    assert!(
        system.lookup(pid_b).await.is_some(),
        "the original must resolve a process spawned through the clone; a spawn \
         that registered somewhere else would leave this lookup empty"
    );

    // And: the name registered through the original resolves, through the clone,
    // to that very process rather than to a same-named process of its own
    let by_name = handle
        .lookup_by_name(&name)
        .await
        .expect("the clone must resolve the name the original registered");
    assert_eq!(
        by_name
            .downcast::<TrackedProcess>()
            .expect("the process registered under this name is a TrackedProcess")
            .pid(),
        pid_a,
        "the name must resolve to the process the original spawned"
    );
}

/// Clones share one dead-letter stream: the same stream identity, and the same
/// delivery.
///
/// The routing half is what makes this more than an accessor test — the
/// dead-letter proxy a spawn hands to its process is captured at spawn time, so
/// a clone carrying its own dead-letter wiring could still report a matching
/// stream pid while dropping the messages somewhere else.
#[tokio::test]
async fn clone_and_original_share_one_dead_letter_stream() {
    // Given: a system and a clone of it
    let system = ProcessSystem::new().await;
    let handle = system.clone();

    // Then: both name the same stream
    assert_eq!(
        handle.dead_letter_stream().pid(),
        system.dead_letter_stream().pid(),
        "a clone must expose the dead-letter stream the original was built with, \
         not a second one"
    );

    // Given: a subscriber registered through the clone
    let count = subscribe_dead_letter_counter(&handle).await;

    // When: a process spawned through the original is stopped and told anyway
    let proxy = spawn_and_stop(&system).await;

    // Then: the dead letter reaches the clone's subscriber
    tell_until_dead_lettered(
        &proxy,
        &count,
        "a dead letter caused through the original must reach the subscriber \
         registered through the clone",
    )
    .await;
}

/// The defaults the builder fixed reach processes spawned through any clone.
///
/// `test_props` leaves the idle timeout at `Inherit`, so the process decays to
/// whatever default the system carries.  A clone that reset its defaults would
/// leave this process with no timeout at all, and it would never stop.
#[tokio::test]
async fn defaults_fixed_by_the_builder_are_observed_through_every_clone() {
    // Given: a system whose default idle timeout was fixed at build time
    let system = ProcessSystem::builder()
        .with_default_idle_timeout(Duration::from_millis(50))
        .build()
        .await;
    let handle = system.clone();

    // When: an Inherit-idle-timeout process is spawned through the clone
    let (started, stopped, counter) = tracked_state();
    let _proxy = handle
        .spawn(test_props(started.clone(), stopped.clone(), counter))
        .await;
    wait_for_flag(&started).await;

    // Then: it decays to the default the builder fixed, so it stops on its own
    wait_for_flag(&stopped).await;
}

// Construction

/// `build()` performs the construction, not just the configuration: the
/// dead-letter stream is running by the time it returns.
#[tokio::test]
async fn build_starts_the_dead_letter_stream() {
    // Given / When
    let system = ProcessSystem::builder().build().await;

    // Then: the stream is registered
    let stream_pid = system.dead_letter_stream().pid();
    assert!(
        system.lookup(stream_pid).await.is_some(),
        "build() must spawn the dead-letter stream, not leave it to be created \
         on first use"
    );

    // And: it is live enough to accept a subscriber
    let _ = subscribe_dead_letter_counter(&system).await;
}

/// The dead-letter infrastructure is not subject to the caller's default idle
/// timeout.
///
/// It is spawned during `build()`, when the configured defaults are already
/// known — which is exactly when handing them to the bootstrap spawn looks like
/// an improvement.  It is not: the dead-letter stream is persistent, and a
/// system that reaps its own dead-letter stream after fifty idle milliseconds
/// silently loses every dead letter from then on.
#[tokio::test]
async fn dead_letter_infrastructure_outlives_the_default_idle_timeout() {
    // Given: a system with a very short default idle timeout
    const DEFAULT_IDLE: Duration = Duration::from_millis(50);
    let system = ProcessSystem::builder()
        .with_default_idle_timeout(DEFAULT_IDLE)
        .build()
        .await;
    let stream_pid = system.dead_letter_stream().pid();

    // When: several times that elapses with nothing sent to the stream
    tokio::time::sleep(DEFAULT_IDLE * 6).await;

    // Then: the stream is still registered
    assert!(
        system.lookup(stream_pid).await.is_some(),
        "the dead-letter stream must not inherit the caller's default idle \
         timeout; the bootstrap spawn keeps its own Inherit-decay values"
    );

    // And: the actor behind it still routes, so a dead letter still arrives
    let count = subscribe_dead_letter_counter(&system).await;
    let proxy = spawn_and_stop(&system).await;
    tell_until_dead_lettered(
        &proxy,
        &count,
        "the dead-letter actor must still route after the default idle timeout \
         has elapsed several times over",
    )
    .await;
}
