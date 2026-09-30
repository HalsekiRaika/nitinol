//! `ProcessSystem::spawn_resident` — one process per resident type, kept alive.
//!
//! Three contracts meet here, all of them owned by `spawn_resident` rather than
//! by the fixture that declares the resident.
//!
//! * **The type is the key, and the key holds once.**  The first call starts the
//!   lifecycle the `Resident` declared; every later call resolves that same
//!   process.  This is deliberately *not* what `ProcessSystem::spawn` does —
//!   `spawn_identity.rs` pins `spawn` as a lifecycle call that never resolves an
//!   existing process, and that contract stays as it is.
//! * **Concurrent callers converge.**  Callers released together must still see
//!   one process between them, so the check-then-start window has to be closed
//!   rather than merely narrow.
//! * **A resident is persistent.**  Idle time must not reap it.  The two
//!   timer-arming paths are `Inherit` decaying to a system default and an
//!   explicit `After` (`process/props/idle_timeout.rs`), so both are exercised —
//!   and no fixture in this file declares `IdleTimeout::Persistent`, because a
//!   fixture that declared it would be testing itself instead of the mechanism.
//!
//! Every assertion is written against the public surface: which pid a call
//! resolves to, how many lifecycles the producer was asked to build, and whether
//! the proxy still gets an answer.

use std::convert::Infallible;
use std::future::Future;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Barrier;

use nitinol_runtime::ident::Pid;
use nitinol_runtime::process::Resident as ResidentViaProcessModule;
use nitinol_runtime::process::{Process, ProcessContext, Receive};
use nitinol_runtime::{IdleTimeout, ProcessSystem, Props, Resident};

// Fixtures

/// The per-test state a resident fixture reports through.
///
/// `Resident::props` is a static method taking no arguments, so a fixture cannot
/// capture anything a test owns.  Each test therefore supplies a marker type
/// implementing this trait: the marker gives the resident a `TypeId` of its own —
/// which is the singleton key — and the statics behind these accessors give the
/// test somewhere to observe its own process without colliding with the tests
/// this binary runs beside it.
trait Knobs: 'static + Send + Sync {
    /// Incremented by the producer, so it counts lifecycles actually started
    /// rather than proxies handed out.
    fn spawns() -> &'static AtomicU32;

    /// Set by `on_stop`, so it distinguishes "still running" from "was reaped".
    fn stopped() -> &'static AtomicBool;

    /// What the fixture declares.  Keeping a resident alive is `spawn_resident`'s
    /// job, so nothing declared here is ever `IdleTimeout::Persistent`.
    fn declared_idle_timeout() -> IdleTimeout;
}

/// Declare a marker type and its [`Knobs`] impl.
///
/// One invocation per test: two tests sharing a marker would share both the
/// singleton key and the counters, and this binary runs its tests in parallel.
/// The statics live inside the accessor bodies so that each marker's impl — not
/// each instantiation of a generic — owns its own pair.
macro_rules! declare_knobs {
    ($marker:ident, $declared_idle_timeout:expr) => {
        struct $marker;

        impl Knobs for $marker {
            fn spawns() -> &'static AtomicU32 {
                static SPAWNS: AtomicU32 = AtomicU32::new(0);
                &SPAWNS
            }

            fn stopped() -> &'static AtomicBool {
                static STOPPED: AtomicBool = AtomicBool::new(false);
                &STOPPED
            }

            fn declared_idle_timeout() -> IdleTimeout {
                $declared_idle_timeout
            }
        }
    };
}

/// A resident process that reports through `K`'s statics.
struct Tracked<K: Knobs> {
    _knobs: PhantomData<K>,
}

impl<K: Knobs> Process for Tracked<K> {
    fn on_stop(&mut self, _ctx: &mut ProcessContext<Self>) -> impl Future<Output = ()> + Send {
        K::stopped().store(true, Ordering::SeqCst);
        async {}
    }
}

impl<K: Knobs> Resident for Tracked<K> {
    fn props() -> Props<Self> {
        Props::new(|| {
            K::spawns().fetch_add(1, Ordering::SeqCst);
            Tracked {
                _knobs: PhantomData,
            }
        })
        .with_idle_timeout(K::declared_idle_timeout())
    }
}

/// Request-response probe: a reply means the process behind the proxy is running.
struct Ping;

impl<K: Knobs> Receive<Ping> for Tracked<K> {
    type Response = ();
    type Error = Infallible;
    async fn recv(
        &mut self,
        _msg: Ping,
        _ctx: &mut ProcessContext<Self>,
    ) -> Result<(), Infallible> {
        Ok(())
    }
}

// Public reachability

declare_knobs!(Reachable, IdleTimeout::Inherit);

/// Both re-export paths name the trait, so a downstream crate can reach it
/// through the crate root or through the `process` module.
///
/// Wiring `process.rs` without `lib.rs` leaves `nitinol_runtime::Resident`
/// unresolved while every runtime assertion below still passes, so the reachable
/// paths are pinned in their own named check rather than left implicit.
#[test]
fn resident_is_named_by_both_public_paths() {
    fn through_crate_root<P: Resident>() {}
    fn through_process_module<P: ResidentViaProcessModule>() {}

    through_crate_root::<Tracked<Reachable>>();
    through_process_module::<Tracked<Reachable>>();
}

// The type is the key, and it holds once

declare_knobs!(OnceIdentity, IdleTimeout::Inherit);

/// A second call resolves the process the first one started.
///
/// The pid is the identity assertion and the producer count is the existence
/// one: a `spawn_resident` that simply forwarded to `spawn` — whose contract is
/// to start a lifecycle every time — would fail both.
#[tokio::test]
async fn second_call_resolves_the_process_the_first_call_started() {
    // Given: a system with no resident of this type yet
    let system = ProcessSystem::new().await;

    // When: the same resident type is requested twice
    let first = system.spawn_resident::<Tracked<OnceIdentity>>().await;
    let second = system.spawn_resident::<Tracked<OnceIdentity>>().await;

    // Then: both proxies address one process
    assert_eq!(
        first.pid(),
        second.pid(),
        "the second call must resolve the resident already running, not start a second one"
    );
    assert_eq!(
        OnceIdentity::spawns().load(Ordering::SeqCst),
        1,
        "exactly one lifecycle may be started for a resident type"
    );
}

declare_knobs!(KeyedFirst, IdleTimeout::Inherit);
declare_knobs!(KeyedSecond, IdleTimeout::Inherit);

/// The key is the *type*: two resident types are two residents in one system.
///
/// This is the half of the key contract that the once-semantics test cannot see.
/// A single-slot cache — one resident per system rather than one per type —
/// satisfies every assertion above, and fails here.
#[tokio::test]
async fn distinct_resident_types_resolve_to_distinct_processes() {
    // Given: one system
    let system = ProcessSystem::new().await;

    // When: two different resident types are requested through it
    let first = system.spawn_resident::<Tracked<KeyedFirst>>().await;
    let second = system.spawn_resident::<Tracked<KeyedSecond>>().await;

    // Then: each type got its own process, and got it once
    assert_ne!(
        first.pid(),
        second.pid(),
        "a second resident type must get its own process, not the first type's"
    );
    assert_eq!(
        KeyedFirst::spawns().load(Ordering::SeqCst),
        1,
        "the first resident type must have started exactly one lifecycle"
    );
    assert_eq!(
        KeyedSecond::spawns().load(Ordering::SeqCst),
        1,
        "the second resident type must have started exactly one lifecycle"
    );
}

declare_knobs!(CloneShared, IdleTimeout::Inherit);

/// Clones of the system share one resident table.
///
/// Cloning a `ProcessSystem` names the same instance (`system_handle_clone.rs`),
/// so a resident requested through a clone is the one the original started.  A
/// table held outside the shared instance would give each handle a resident of
/// its own while still satisfying the two-calls-through-one-handle test.
#[tokio::test]
async fn a_clone_resolves_the_resident_the_original_started() {
    // Given: a system and a clone of it
    let system = ProcessSystem::new().await;
    let handle = system.clone();

    // When: the same resident type is requested through each
    let through_original = system.spawn_resident::<Tracked<CloneShared>>().await;
    let through_clone = handle.spawn_resident::<Tracked<CloneShared>>().await;

    // Then: both name one process
    assert_eq!(
        through_original.pid(),
        through_clone.pid(),
        "a clone must resolve the resident the original started, not start its own"
    );
    assert_eq!(
        CloneShared::spawns().load(Ordering::SeqCst),
        1,
        "one resident type on one system may start only one lifecycle, whichever handle asks"
    );
}

// Concurrent callers converge

declare_knobs!(Concurrent, IdleTimeout::Inherit);

/// Callers released together still get one process between them.
///
/// The barrier is what makes this more than a repeat of the sequential test: it
/// holds every caller until all of them are ready, so the calls overlap inside
/// the check-then-start window instead of arriving after it has closed.  An
/// implementation that checked under a read lock and started under a write lock
/// it re-acquired would leave that window open and start several lifecycles here.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_callers_converge_on_one_process() {
    // Given: a system, and callers waiting on a barrier
    const CALLERS: usize = 8;
    let system = ProcessSystem::new().await;
    let gate = Arc::new(Barrier::new(CALLERS));

    // When: the barrier releases them into the same call
    let mut callers = Vec::with_capacity(CALLERS);
    for _ in 0..CALLERS {
        let system = system.clone();
        let gate = Arc::clone(&gate);
        callers.push(tokio::spawn(async move {
            gate.wait().await;
            system.spawn_resident::<Tracked<Concurrent>>().await.pid()
        }));
    }
    let mut pids: Vec<Pid> = Vec::with_capacity(CALLERS);
    for caller in callers {
        pids.push(
            caller
                .await
                .expect("every spawn_resident caller task must run to completion"),
        );
    }

    // Then: every caller resolved to the same process
    let resolved = pids[0];
    assert!(
        pids.iter().all(|pid| *pid == resolved),
        "all concurrent callers must receive a proxy to one process, got {pids:?}"
    );
    assert_eq!(
        Concurrent::spawns().load(Ordering::SeqCst),
        1,
        "concurrent calls must converge on one lifecycle, not start one each"
    );
}

// A resident is persistent

declare_knobs!(InheritedIdleTimeout, IdleTimeout::Inherit);

/// The system default idle timeout must not reap a resident.
///
/// The fixture leaves its declaration at `Inherit`, so `spawn_resident` is the
/// only thing that can keep this process alive: an implementation that passed
/// `props()` through unchanged would let it decay to the default the builder
/// fixed and be reaped, exactly as `idle_timeout.rs` pins for an ordinary spawn.
#[tokio::test]
async fn resident_outlives_the_system_default_idle_timeout() {
    // Given: a system whose default idle timeout is short, and a resident that
    // declares nothing about idle timeouts
    const DEFAULT_IDLE: Duration = Duration::from_millis(50);
    let system = ProcessSystem::builder()
        .with_default_idle_timeout(DEFAULT_IDLE)
        .build()
        .await;
    let proxy = system
        .spawn_resident::<Tracked<InheritedIdleTimeout>>()
        .await;

    // When: several times the default elapses with nothing sent to it
    tokio::time::sleep(DEFAULT_IDLE * 4).await;

    // Then: it was not reaped
    assert!(
        !InheritedIdleTimeout::stopped().load(Ordering::SeqCst),
        "a resident must not stop on the system default idle timeout"
    );

    // And: it still answers through the proxy the call handed back
    proxy
        .ask(Ping)
        .await
        .expect("the resident must still answer through its proxy after idling");
}

declare_knobs!(
    DeclaredIdleTimeout,
    IdleTimeout::After(Duration::from_millis(50))
);

/// A resident's own `After` declaration must not reap it either.
///
/// Persistence is stated unconditionally, and `After` is the second of the two
/// ways a timer can arm (`process/props/idle_timeout.rs`).  An implementation
/// that only rescued `Inherit` — leaving an explicit `After` honoured — passes
/// the test above and fails here.
#[tokio::test]
async fn resident_outlives_the_idle_timeout_its_own_props_declared() {
    // Given: a resident whose props declare a 50ms idle timeout of their own
    let system = ProcessSystem::new().await;
    let proxy = system
        .spawn_resident::<Tracked<DeclaredIdleTimeout>>()
        .await;

    // When: several times that declaration elapses with nothing sent to it
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Then: it was not reaped
    assert!(
        !DeclaredIdleTimeout::stopped().load(Ordering::SeqCst),
        "a resident must not stop on the idle timeout its own props declared"
    );

    // And: it still answers through the proxy the call handed back
    proxy
        .ask(Ping)
        .await
        .expect("the resident must still answer through its proxy after idling");
}
