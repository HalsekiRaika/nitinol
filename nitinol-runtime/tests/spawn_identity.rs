//! `ProcessSystem::spawn` starts a lifecycle; it does not resolve one.
//!
//! Get-or-activate belongs to the layer above (R-4): `spawn` stays an
//! explicit lifecycle call, so two spawns of the same declaration are two
//! processes.  Folding a lookup into `spawn` would make that indistinguishable
//! from a resolve and silently change what every existing caller gets.

#[path = "common/helpers.rs"]
mod common;

use nitinol_runtime::ident::ProcessName;
use nitinol_runtime::ProcessSystem;

use common::{test_props, tracked_state, GetCount, Increment};

/// Two spawns of the same `Props` are two processes with two identities.
#[tokio::test]
async fn spawn_starts_a_new_process_for_every_call() {
    // Given
    let system = ProcessSystem::new().await;
    let (started, stopped, counter) = tracked_state();

    // When
    let first = system
        .spawn(test_props(
            started.clone(),
            stopped.clone(),
            counter.clone(),
        ))
        .await;
    let second = system.spawn(test_props(started, stopped, counter)).await;

    // Then
    assert_ne!(
        first.pid(),
        second.pid(),
        "spawn must start a lifecycle, not resolve an existing one"
    );
}

/// Naming a process does not turn `spawn` into a lookup either: the second call
/// still starts its own process, with its own state.
///
/// The counters are the discriminator — a `spawn` that had returned the first
/// process would report both increments through either handle.
#[tokio::test]
async fn spawn_under_a_taken_name_still_starts_a_new_process() {
    // Given
    let system = ProcessSystem::new().await;
    let name = ProcessName::new("spawn-identity-taken-name");
    let (started_a, stopped_a, counter_a) = tracked_state();
    let (started_b, stopped_b, counter_b) = tracked_state();

    let first = system
        .spawn(test_props(started_a, stopped_a, counter_a).with_name(name.clone()))
        .await;
    let second = system
        .spawn(test_props(started_b, stopped_b, counter_b).with_name(name))
        .await;

    // When
    first.tell(Increment).await.expect("tell to the first");
    second.tell(Increment).await.expect("tell to the second");

    // Then
    assert_ne!(
        first.pid(),
        second.pid(),
        "a taken name must not make spawn hand back the process that holds it"
    );
    assert_eq!(
        first.ask(GetCount).await.expect("ask the first"),
        1,
        "the first process must have counted only its own message"
    );
    assert_eq!(
        second.ask(GetCount).await.expect("ask the second"),
        1,
        "the second process must have counted only its own message"
    );
}
