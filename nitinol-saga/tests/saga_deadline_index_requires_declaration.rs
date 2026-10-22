//! A host starts the deadline index exactly when the saga type has deadlines,
//! one top-level process per partition.
//!
//! ```gherkin
//!   Scenario: a saga type that declares no deadline
//!     Given a saga type whose alphabet has no deadline variant
//!     When it is hosted
//!     Then no index process is started
//!
//!   Scenario: a saga type that declares one
//!     Given a saga type whose alphabet has a deadline variant
//!     When it is hosted with P partitions
//!     Then P index processes run on the hosting system
//!     And stopping the manager leaves them running
//! ```
//!
//! The index is reached through the handle the host hands back, as the manager
//! and the delivery host are: its pids are what the system's registry is asked
//! about. A saga type with no deadline has no index to name. That it also has
//! nothing reading schedule facts is read off the journal, since an index that
//! ran without being named would still have to ask the store for them.

#[path = "common/helpers.rs"]
mod common;
#[path = "common/deadline_index.rs"]
mod deadline_index;
use deadline_index::{
    fired, is_lifecycle_load, is_schedule_load, partition_count, probe_system, spawn_index_saga,
    system_over, wait_until_queries, ProbeJournal, Recorded, Trigger, PROBE_INTERVAL,
    PROBE_TIMEOUT, TEST_POLL_INTERVAL,
};

use std::collections::HashSet;
use std::time::Instant;

use async_trait::async_trait;

use nitinol_eventsource::Event;
use nitinol_persistence::store::{InMemoryEventStore, InMemoryStore};
use nitinol_persistence::LoadQuery;
use nitinol_runtime::ident::Pid;
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{
    Saga, SagaContext, SagaEffect, SagaId, SagaManagerProps, SagaSubscribe, Subscription,
};

/// An alphabet with one observed source and no deadline variant.
#[derive(SagaSubscribe)]
enum PlainInput {
    Trigger(Trigger),
}

/// A saga with no time of its own.
struct PlainSaga;

#[async_trait]
impl Saga for PlainSaga {
    type Subscribe = PlainInput;
    type Event = Recorded;
    type Error = std::convert::Infallible;

    fn correlate(input: &Self::Subscribe) -> Option<SagaId> {
        let PlainInput::Trigger(trigger) = input;
        Some(SagaId::new(trigger.saga.clone()))
    }

    async fn handle(
        &mut self,
        _input: Self::Subscribe,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        Ok(SagaEffect::empty())
    }
}

/// The delivery host's load: the one naming the saga's own event type. Each one
/// is a tick of a host that is running, which is the barrier the absence below
/// is read against.
fn is_delivery_load(query: &LoadQuery) -> bool {
    query.event_type_prefix.as_ref() == Some(&<Recorded as Event>::EVENT_TYPE.to_path())
}

async fn is_running(ps: &ProcessSystem, pid: Pid) -> bool {
    ps.lookup(pid).await.is_some()
}

#[tokio::test]
async fn a_saga_type_without_a_deadline_variant_starts_no_index() {
    // Given
    let probe = ProbeJournal::recording(InMemoryEventStore::default());
    let system = probe_system(&probe).await;

    // When
    let host = SagaManagerProps::<PlainSaga>::new(|_| PlainSaga)
        .observing::<Trigger>(Subscription::stream("deadline-index-plain-upstream"))
        .with_delivery_poll_interval(TEST_POLL_INTERVAL)
        .spawn(&system)
        .await;

    // Then
    assert!(
        host.deadline_index().is_none(),
        "a saga type whose alphabet declares no deadline has no index to start, \
         so the handle names none"
    );

    wait_until_queries(
        &probe,
        is_delivery_load,
        5,
        "the delivery host must keep polling, so the host is running",
    )
    .await;
    let index_loads: Vec<LoadQuery> = probe
        .queries()
        .into_iter()
        .filter(|query| is_schedule_load(query) || is_lifecycle_load(query))
        .collect();
    assert!(
        index_loads.is_empty(),
        "nothing may read schedule or lifecycle facts for a saga type with no \
         deadline; the journal was asked {index_loads:?}"
    );
}

#[tokio::test]
async fn a_saga_type_with_a_deadline_variant_starts_one_index_process_per_partition() {
    // Given
    let store = InMemoryStore::default();
    let system = system_over(&store).await;
    let partitions = 3;

    // When
    let host = spawn_index_saga(
        &system,
        Subscription::stream("deadline-index-declared-upstream"),
        partitions,
        &fired(),
    )
    .await;

    // Then
    let index = host
        .deadline_index()
        .expect("a saga type whose alphabet declares a deadline must have its index started");
    let pids = index.pids();
    assert_eq!(
        pids.len(),
        partition_count(partitions).get(),
        "the index shares the delivery host's partition count: one process per \
         partition; pids were {pids:?}"
    );
    let distinct: HashSet<Pid> = pids.iter().copied().collect();
    assert_eq!(
        distinct.len(),
        pids.len(),
        "each partition is a process of its own; pids were {pids:?}"
    );
    assert!(
        !distinct.contains(&host.manager().pid()),
        "the index is not the manager; pids were {pids:?}"
    );
    for pid in pids {
        assert!(
            is_running(system.process_system(), *pid).await,
            "index partition {pid:?} must run on the system the saga was hosted on"
        );
    }
}

#[tokio::test]
async fn stopping_the_manager_leaves_the_index_partitions_running() {
    // Given
    let store = InMemoryStore::default();
    let system = system_over(&store).await;
    let host = spawn_index_saga(
        &system,
        Subscription::stream("deadline-index-stop-upstream"),
        2,
        &fired(),
    )
    .await;
    let pids: Vec<Pid> = host
        .deadline_index()
        .expect("a saga type whose alphabet declares a deadline must have its index started")
        .pids()
        .to_vec();
    let ps = system.process_system();

    // When
    host.manager()
        .stop()
        .await
        .expect("stopping a running manager must be accepted");
    let deadline = Instant::now() + PROBE_TIMEOUT;
    while is_running(ps, host.manager().pid()).await {
        assert!(
            Instant::now() < deadline,
            "the manager must leave the registry once it is stopped"
        );
        tokio::time::sleep(PROBE_INTERVAL).await;
    }

    // Then
    let mut stopped = Vec::new();
    for pid in &pids {
        if !is_running(ps, *pid).await {
            stopped.push(*pid);
        }
    }
    assert!(
        stopped.is_empty(),
        "the index partitions are top-level processes of the system, not \
         children of the manager, so stopping the manager must not stop them; \
         stopped {stopped:?} of {pids:?}"
    );
}
