//! The scheduler is started by type, and the type holds one of it.
//!
//! `SchedulerProcess` is a resident: callers reach it through
//! `ProcessSystem::spawn_resident::<SchedulerProcess>()` rather than through a
//! spawn function of its own, so no caller has to know whether someone else
//! already started it.  Every saga in a system shares one timer core, and that
//! sharing is what this file observes — two asks, one process.
//!
//! The runtime's own `spawn_resident` contracts (type keying, concurrent
//! convergence, persistence) are pinned in `nitinol-runtime/tests/spawn_resident.rs`.
//! What belongs here is that `nitinol-saga` declares its scheduler as a
//! resident at all, and that the proxy the ask hands back converts into the
//! `SchedulerProxy` the saga builders take.
//!
//! The functional half of that conversion — a timer registered through the
//! converted proxy actually firing — is `saga_deadline_scheduler.rs`, which
//! reaches its scheduler through this same path.
//!
//! Run with: `cargo test -p nitinol-saga --test saga_scheduler_resident`.

use nitinol_runtime::process::ProcessProxy;
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{SchedulerProcess, SchedulerProxy};

/// The one step between what the ask hands back and what the saga builders
/// take.
///
/// Pinned on its own because every runtime assertion below still passes without
/// it: removing `spawn_scheduler` leaves `SchedulerProxy` with no public
/// constructor, and then `with_scheduler` — which this task keeps — has nothing
/// to be given.
#[test]
fn a_resident_scheduler_proxy_converts_into_the_builders_handle() {
    fn accepts<P: Into<SchedulerProxy>>() {}

    accepts::<ProcessProxy<SchedulerProcess>>();
}

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

    // Then: identity is read off the runtime proxy rather than off
    // `SchedulerProxy`, which deliberately exposes none — it is a
    // timer-operations handle, not an addressable process.
    assert_eq!(
        first.pid(),
        second.pid(),
        "a second ask for the resident scheduler must resolve the one already \
         running, not start a second timer core"
    );
}
