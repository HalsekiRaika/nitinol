//! A partition that is restarted resumes from what the store holds now.
//!
//! ```gherkin
//!   Scenario: a partition restarted under supervision
//!     Given a running delivery partition
//!     And a recorded position that has since moved on
//!     When the partition fails and is restarted
//!     Then its next query resumes above the recorded position as it now stands
//! ```
//!
//! The same partition lives across the restart — it is the host the wiring
//! started, supervised — so what it resumes from has to be read then rather
//! than settled once at startup. A position captured when the host was wired
//! would send a restarted partition back to where it began, re-delivering a
//! stretch an operator may have deliberately passed over.
//!
//! The journal is left empty on purpose: with nothing to deliver, the host
//! records no position of its own, so the only position in the store is the one
//! put there from outside and the query that follows the restart can only have
//! come from reading it.

#[path = "common/delivery.rs"]
mod delivery;
use delivery::{
    delivery_id, delivery_system, partition_count, scope_of, wait_until_loads, DeliverySaga,
    RecordingEventStore, RecordingOffsetStore, Triggered, TRIGGER_KEY,
};

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use nitinol_persistence::error::{AppendError, LoadError};
use nitinol_persistence::store::{EventStore, EventStream, InMemoryEventStore};
use nitinol_persistence::{AppendOutcome, AppendingEvent, LoadQuery};
use nitinol_saga::{SagaManagerProps, Subscription};

/// Slow enough that the position below is in the store well before the query
/// that is meant to read it.
const POLL: Duration = Duration::from_millis(200);

/// The delivery host's load call that fails, so the partition is restarted
/// under supervision.
const FAILING_CALL: usize = 3;

/// A position no host in this test could have reached on its own.
const MOVED_ON_TO: u64 = 5;

/// A store that panics on the delivery host's `nth` load, and hands every call
/// to the recorder it wraps.
///
/// Counted over the host's own loads rather than over every call the store saw:
/// one journal carries the manager's source poller too, so a fault numbered
/// over all of them would take down whichever consumer happened to ask third.
/// A host's load is the one naming the decision event type as a prefix, which
/// is the same thing [`RecordingEventStore`] reads its record of calls by.
#[derive(Clone)]
struct PanicOnHostLoad {
    inner: RecordingEventStore,
    nth: usize,
    host_loads: Arc<AtomicUsize>,
}

impl PanicOnHostLoad {
    fn new(inner: RecordingEventStore, nth: usize) -> Self {
        Self {
            inner,
            nth,
            host_loads: Arc::new(AtomicUsize::new(0)),
        }
    }
}

#[async_trait]
impl EventStore for PanicOnHostLoad {
    async fn append(
        &self,
        key: &str,
        events: Vec<AppendingEvent>,
    ) -> Result<AppendOutcome, AppendError> {
        self.inner.append(key, events).await
    }

    async fn load(&self, query: LoadQuery) -> Result<EventStream<'_>, LoadError> {
        let host_load = query.event_type_prefix.is_some();
        let nth = if host_load {
            self.host_loads.fetch_add(1, Ordering::SeqCst) + 1
        } else {
            0
        };
        // Handed on before the fault, so the call that takes the partition down
        // is in the record the clause below reads.
        let loaded = self.inner.load(query).await;
        if host_load && nth == self.nth {
            panic!("the fixture store panics on the host's load call {nth}");
        }
        loaded
    }
}

#[tokio::test]
async fn a_restarted_partition_resumes_from_the_position_now_recorded() {
    // Given
    let recording = RecordingEventStore::new(InMemoryEventStore::default());
    let offsets = RecordingOffsetStore::new();
    let journal = PanicOnHostLoad::new(recording.clone(), FAILING_CALL);
    let system = delivery_system(&journal, &offsets).await;

    let _host = SagaManagerProps::<DeliverySaga>::new(|_| DeliverySaga)
        .observing::<Triggered>(Subscription::stream(TRIGGER_KEY))
        .with_delivery_id(delivery_id())
        .with_partitions(partition_count(1))
        .with_delivery_poll_interval(POLL)
        .spawn(&system)
        .await;

    wait_until_loads(&recording, 1, "the partition must start polling").await;

    // And: the recorded position moves on while the partition is running.
    offsets.put(&scope_of(0), MOVED_ON_TO);

    // When: the next-but-one load takes the partition down, and supervision
    // brings it back.
    let queries = wait_until_loads(
        &recording,
        FAILING_CALL + 2,
        "the partition must be restarted and poll again",
    )
    .await;

    // Then
    let before = &queries[..FAILING_CALL - 1];
    let after = &queries[FAILING_CALL..];

    assert!(
        before
            .iter()
            .all(|query| query.from_global_sequence == Some(1)),
        "the running partition kept its own position while it was alive: a \
         position written from outside is not a message to it; queries were \
         {before:?}"
    );
    assert!(
        after
            .iter()
            .all(|query| query.from_global_sequence == Some(MOVED_ON_TO + 1)),
        "after the restart the partition resumes above the position the store \
         holds now, which is the only place {} could have come from; queries \
         were {after:?}",
        MOVED_ON_TO + 1
    );
}
