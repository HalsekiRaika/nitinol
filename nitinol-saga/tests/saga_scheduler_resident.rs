//! The scheduler is started by type, and the type holds one of it.
//!
//! `SchedulerProcess` is a resident: callers reach it through
//! `ProcessSystem::spawn_resident::<SchedulerProcess>()` rather than through a
//! spawn function of its own, so no caller has to know whether someone else
//! already started it.  Every saga in a system shares one timer core, and that
//! sharing is what this file observes — two asks, one process.
//!
//! The runtime's own `spawn_resident` contracts (type keying, concurrent
//! convergence, persistence) are pinned in
//! `nitinol-runtime/tests/spawn_resident.rs`.  What belongs here is that
//! `nitinol-saga` declares its scheduler as a resident at all.
//!
//! Who makes that ask is no longer the caller's business: a saga whose alphabet
//! declares a `#[subscribe(deadline)]` variant has the resident core resolved
//! for it at spawn, which is pinned in `saga_host_owns_scheduler.rs` — including
//! that several such sagas converge on the one core this file asks for twice.
//! The functional half — a deadline registered through the resolved core
//! actually firing — is `saga_deadline_scheduler.rs`.
//!
//! Run with: `cargo test -p nitinol-saga --test saga_scheduler_resident`.

use nitinol_runtime::ProcessSystem;
use nitinol_saga::SchedulerProcess;

/// Given a system that has never been asked for a scheduler,
/// when the resident scheduler is asked for twice — once through the system and
/// once through a clone of it —
/// then both asks name one process.
///
/// The pid is the whole assertion: a declaration that forwarded to
/// `ProcessSystem::spawn`, whose contract is to start a lifecycle on every
/// call, hands back two processes here and leaves each saga wired to a timer
/// core of its own.
#[tokio::test]
async fn spawn_resident_resolves_one_scheduler() {
    // Given
    let system = ProcessSystem::new().await;
    let handle = system.clone();

    // When
    let first = system.spawn_resident::<SchedulerProcess>().await;
    let second = handle.spawn_resident::<SchedulerProcess>().await;

    // Then: identity is read off the runtime proxy, which is the only handle
    // this ask hands out — the timer-operations handle it wraps is the hosting
    // machinery's and is not part of the crate's surface.
    assert_eq!(
        first.pid(),
        second.pid(),
        "a second ask for the resident scheduler must resolve the one already \
         running, not start a second timer core"
    );
}
