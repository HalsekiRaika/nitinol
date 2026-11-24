//! A manager stopped while one of its source partitions is asking it still
//! stops.
//!
//! ```gherkin
//!   Scenario: the manager is stopped in the middle of a partition's read
//!     Given a source partition handing the manager two records of one read
//!     And the manager busy answering the first
//!     When the manager is stopped
//!     Then the manager, and every partition it ran, leaves the registry
//! ```
//!
//! A source partition asks the manager from inside its own `apply`, and asks
//! again for the next record as soon as the first is answered, without
//! reading its signals in between. The manager answers the first record after
//! its stop has been requested, so the partition's second ask reaches a
//! manager that has left its loop. The manager waits for its partitions to
//! stop, so that ask must come back unanswered rather than wait for a reply
//! the stopping manager will never give.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use tokio::sync::Notify;

use nitinol_runtime::ident::Pid;
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{Saga, SagaContext, SagaEffect, SagaId, SagaManagerProps};

use super::delivery;
use super::manager_offsets;
use delivery::{
    delivery_system, RecordingOffsetStore, PROBE_INTERVAL, PROBE_TIMEOUT, TEST_POLL_INTERVAL,
};
use manager_offsets::{
    append_placed, from_stream, host_id, partitions, quiet, Acted, Placed, Refused, Shipped,
    SourceJournal, Upstream,
};

const SOURCE: &str = "manager-stop-while-asked";

const SAGA: &str = "manager-stop-while-asked-instance";

/// Holds the first record it handles until the test lets it through, and
/// settles every record by its position alone.
struct HeldSaga {
    held: Arc<AtomicBool>,
    entered: Arc<AtomicBool>,
    release: Arc<Notify>,
}

#[async_trait]
impl Saga for HeldSaga {
    type Subscribe = Upstream;
    type Event = Acted;
    type Error = Refused;

    fn correlate(input: &Self::Subscribe) -> Option<SagaId> {
        match input {
            Upstream::Placed(placed) => Some(SagaId::new(placed.saga.clone())),
            Upstream::Shipped(shipped) => Some(SagaId::new(shipped.saga.clone())),
        }
    }

    async fn handle(
        &mut self,
        _input: Self::Subscribe,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        if !self.held.swap(true, Ordering::SeqCst) {
            self.entered.store(true, Ordering::SeqCst);
            self.release.notified().await;
        }
        Ok(SagaEffect::None)
    }
}

async fn wait_until_entered(entered: &AtomicBool) {
    let deadline = Instant::now() + PROBE_TIMEOUT;
    while !entered.load(Ordering::SeqCst) {
        assert!(
            Instant::now() < deadline,
            "the first record must reach handle"
        );
        tokio::time::sleep(PROBE_INTERVAL).await;
    }
}

async fn wait_until_gone(ps: &ProcessSystem, pid: Pid) {
    let deadline = Instant::now() + PROBE_TIMEOUT;
    while ps.lookup(pid).await.is_some() {
        assert!(
            Instant::now() < deadline,
            "the manager stopped while its partition asks it must leave the registry"
        );
        tokio::time::sleep(PROBE_INTERVAL).await;
    }
}

#[tokio::test]
async fn a_manager_stopped_while_its_partition_asks_it_still_stops() {
    // Given: two records one read of the only partition hands on in turn
    let journal = SourceJournal::recording();
    let offsets = RecordingOffsetStore::new();
    let system = delivery_system(&journal, &offsets).await;
    append_placed(&journal, SOURCE, 1, SAGA, "first").await;
    append_placed(&journal, SOURCE, 2, SAGA, "second").await;

    let held = Arc::new(AtomicBool::new(false));
    let entered = Arc::new(AtomicBool::new(false));
    let release = Arc::new(Notify::new());
    let host = SagaManagerProps::<HeldSaga>::new({
        let held = Arc::clone(&held);
        let entered = Arc::clone(&entered);
        let release = Arc::clone(&release);
        move |_components| HeldSaga {
            held: Arc::clone(&held),
            entered: Arc::clone(&entered),
            release: Arc::clone(&release),
        }
    })
    .observing::<Placed>(from_stream(SOURCE))
    .observing::<Shipped>(quiet())
    .with_delivery_id(host_id())
    .with_partitions(partitions(1))
    .with_delivery_poll_interval(TEST_POLL_INTERVAL)
    .spawn(&system)
    .await;

    // And: the manager is busy answering the first record
    wait_until_entered(&entered).await;

    // When: the manager is stopped, and then answers the first record
    host.manager()
        .stop()
        .await
        .expect("a running manager accepts a stop");
    release.notify_one();

    // Then: the manager — which leaves only after its partitions — is gone
    wait_until_gone(system.process_system(), host.manager().pid()).await;
}
