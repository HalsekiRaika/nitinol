//! The delivery host's settings are named on the `host` builder, and its
//! operator entry is reached through the handle `host` hands back.
//!
//! ```gherkin
//!   Scenario: the identifier is named on the host builder
//!     Given a saga hosted with a delivery identifier named
//!     When the host records progress
//!     Then it is filed under that identifier, and under nothing else
//!
//!   Scenario: the partition count is named on the host builder
//!     Given a saga hosted with two delivery partitions named
//!     When the host records progress
//!     Then each partition records under its own key
//!
//!   Scenario: the delivery poll interval is named on the host builder
//!     Given a saga hosted with a delivery poll interval named, observing a
//!           source that names none
//!     When it runs for a while with nothing to deliver
//!     Then the gap between the delivery host's queries is at least that
//!          interval
//!     And the observed source keeps the cadence it had: the interval named for
//!         delivery reaches the delivery host and nothing else
//!
//!   Scenario: the operator entry is reached through the handle
//!     Given a hosted saga whose delivery host is held on a refused delivery
//!     When an operator moves that partition past the hold through the handle
//!     Then the delivery behind the hold is carried out
//! ```
//!
//! What each setting *means* belongs to `SagaDeliveryProps` and is fixed by the
//! `saga_delivery_*.rs` tests, which wire a host from stores named by hand.
//! This file fixes the half those cannot see: that a value named on the saga's
//! own builder reaches the host that builder started. A setting dropped on the
//! way, or applied to the manager's subscriptions instead, leaves a host running
//! on a default — which no amount of asserting the default would notice.
//!
//! The two intervals are deliberately observed together. `with_delivery_poll_interval`
//! and `Subscription::with_poll_interval` are a pull consumer's cadence on two
//! different stores, and the names are close enough that a wiring applying one
//! where the other was meant has to be visible: a source that names no interval
//! must keep polling at the framework's own cadence while the delivery host
//! polls at the slow one named for it.
//!
//! The manager is inert throughout: the fixture sagas' `correlate` answers
//! `None` and nothing is ever appended to the observed source, so every
//! decision the host reads was seeded on the journal directly and the manager
//! contributes no load of its own. The observed source is given a store of its
//! own for the same reason — the loads these clauses read on the system's
//! journal are the delivery host's.
//!
//! Run with:
//! `cargo test -p nitinol-saga --test saga_host_delivery_configuration`.

#[path = "common/delivery.rs"]
mod delivery;
use delivery::{
    append_decided, delivery_id, delivery_persistence, operations_of, partition_count, scope_of,
    settle, wait_until_operations, wait_until_stored, DeliverySaga, DeliveryStore, Ledger,
    RecordingEventStore, RecordingOffsetStore, SelectiveSaga, Triggered, PROBE_INTERVAL,
    PROBE_TIMEOUT, REFUSED_SUFFIX, TEST_POLL_INTERVAL,
};

use std::time::{Duration, Instant};

use nitinol_eventsource::system::EventSourceSystem;
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::AggregateId;
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{SagaId, SagaSystemExt, Subscription};

/// Deliberately far from the framework's own delivery cadence, and far enough
/// above it that a host left on a default would be caught by the gaps below.
const SLOW: Duration = Duration::from_millis(500);

/// Long enough for several slow polls, so the gaps are about the interval
/// rather than about startup.
const WINDOW: Duration = Duration::from_millis(1_800);

/// The stream the fixture sagas' one declared source is bound to. Nothing is
/// ever appended to it.
const UPSTREAM_KEY: &str = "saga-host-delivery-configuration-upstream";

/// A system whose provider supplies `journal` as the store every stream is kept
/// in and `offsets` as the saga-offset slot a delivery host records in.
async fn system_over<E>(
    journal: &E,
    offsets: &RecordingOffsetStore,
) -> EventSourceSystem<DeliveryStore>
where
    E: EventStore + Clone + 'static,
{
    EventSourceSystem::builder(ProcessSystem::new().await)
        .with_persistence(delivery_persistence(journal, offsets))
        .build()
}

/// A declared source on a store of its own, so a clause reading the system's
/// journal reads the delivery host's loads and no others.
fn quiet_source() -> Subscription {
    Subscription::stream(
        InMemoryEventStore::default(),
        &AggregateId::new(UPSTREAM_KEY),
    )
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

#[tokio::test]
async fn the_identifier_named_on_the_host_builder_is_the_one_progress_is_filed_under() {
    // Given
    let journal = InMemoryEventStore::default();
    let offsets = RecordingOffsetStore::new();
    let system = system_over(&journal, &offsets).await;

    let target = AggregateId::new("ledger-host-named-id");
    let position = append_decided(
        &journal,
        &SagaId::new("instance-1"),
        1,
        target.as_str(),
        "op-1",
    )
    .await;

    // When
    let host = system
        .host::<DeliverySaga>(|_components| DeliverySaga)
        .observing::<Triggered>(quiet_source())
        .with_delivery_id(delivery_id())
        .with_delivery_poll_interval(TEST_POLL_INTERVAL)
        .spawn()
        .await;

    // Then
    assert_eq!(
        host.delivery().delivery_id(),
        &delivery_id(),
        "the host carries the identifier the saga's own builder named"
    );
    assert_eq!(
        wait_until_stored(
            &offsets,
            &scope_of(0),
            position,
            "progress must be filed under the identifier the host builder named"
        )
        .await,
        position,
    );
    assert_eq!(
        offsets.keys(),
        vec![scope_of(0).as_str().to_owned()],
        "and under nothing else: a derived identifier beside it would mean the \
         value named on the builder never reached the host"
    );
}

#[tokio::test]
async fn the_partition_count_named_on_the_host_builder_is_the_one_progress_is_spread_over() {
    // Given: one instance per partition, both delivering to receivers that
    // accept.
    let journal = InMemoryEventStore::default();
    let offsets = RecordingOffsetStore::new();
    let system = system_over(&journal, &offsets).await;

    append_decided(
        &journal,
        &SagaId::new("instance-1"),
        1,
        "ledger-host-partitions-a",
        "op-a",
    )
    .await;
    let last = append_decided(
        &journal,
        &SagaId::new("instance-2"),
        1,
        "ledger-host-partitions-b",
        "op-b",
    )
    .await;

    // When
    let _host = system
        .host::<DeliverySaga>(|_components| DeliverySaga)
        .observing::<Triggered>(quiet_source())
        .with_delivery_id(delivery_id())
        .with_partitions(partition_count(2))
        .with_delivery_poll_interval(TEST_POLL_INTERVAL)
        .spawn()
        .await;

    // Then
    for partition in [0usize, 1] {
        assert_eq!(
            wait_until_stored(
                &offsets,
                &scope_of(partition),
                last,
                "each partition the host builder asked for must record its own \
                 progress"
            )
            .await,
            last,
            "partition {partition} finishes with every record the subscription \
             returned — its own by delivering, the others by owing nothing"
        );
    }

    settle().await;

    assert_eq!(
        offsets.keys(),
        vec![
            scope_of(0).as_str().to_owned(),
            scope_of(1).as_str().to_owned(),
        ],
        "the count named on the saga's own builder is the number of partitions \
         the host runs: a count that never reached it would leave the single \
         key a host has by default"
    );
}

#[tokio::test]
async fn the_delivery_interval_named_on_the_host_builder_reaches_the_delivery_host_alone() {
    // Given: nothing to deliver and nothing upstream, so every load either
    // store records is a poll rather than work.
    let journal = RecordingEventStore::new(InMemoryEventStore::default());
    let upstream = RecordingEventStore::new(InMemoryEventStore::default());
    let offsets = RecordingOffsetStore::new();
    let system = system_over(&journal, &offsets).await;

    // When: the delivery host is given the slow interval, and the observed
    // source names none at all.
    let _host = system
        .host::<DeliverySaga>(|_components| DeliverySaga)
        .observing::<Triggered>(Subscription::stream(
            upstream.clone(),
            &AggregateId::new(UPSTREAM_KEY),
        ))
        .with_delivery_id(delivery_id())
        .with_delivery_poll_interval(SLOW)
        .spawn()
        .await;
    tokio::time::sleep(WINDOW).await;

    // Then: the journal's loads are the delivery host's, and they are spaced by
    // the interval the builder named.
    let queries = journal.queries();
    assert!(
        queries
            .iter()
            .all(|query| query.event_type_prefix.is_some()),
        "every load of the journal in this clause has to be the delivery host's \
         own subscription, or the gaps below measure something else; the \
         queries were {queries:?}"
    );

    let gaps = journal.load_gaps();
    assert!(
        gaps.len() >= 2,
        "the delivery host must poll more than once in {WINDOW:?}; it polled {} \
         times",
        journal.load_count()
    );
    for (nth, gap) in gaps.iter().enumerate() {
        assert!(
            *gap >= SLOW - Duration::from_millis(50),
            "gap {nth} between the delivery host's queries was {gap:?}, shorter \
             than the {SLOW:?} the host builder named: the interval is not \
             reaching the delivery host"
        );
    }

    // And: the observed source, which named no interval, keeps its own cadence
    let upstream_gaps = upstream.load_gaps();
    assert!(
        upstream_gaps.len() >= 2,
        "the observed source must still poll: it polled {} times in {WINDOW:?}, \
         so a delivery interval is being read as this source's cadence too",
        upstream.load_count()
    );
    for (nth, gap) in upstream_gaps.iter().enumerate() {
        assert!(
            *gap < SLOW,
            "gap {nth} of the observed source was {gap:?} — as long as the \
             {SLOW:?} named for delivery.  The delivery interval reaches the \
             delivery host and no other consumer; a source's own cadence is \
             `Subscription::with_poll_interval`"
        );
    }
}

#[tokio::test]
async fn an_operator_reaches_a_held_partition_through_the_handle() {
    // Given: a hosted saga whose delivery host is held on a refused delivery,
    // with a reachable delivery behind it.
    let journal = RecordingEventStore::new(InMemoryEventStore::default());
    let offsets = RecordingOffsetStore::new();
    let system = system_over(&journal, &offsets).await;

    let saga_id = SagaId::new("instance-1");
    let refused_target = format!("host-escape-target{REFUSED_SUFFIX}");
    let live_target = AggregateId::new("ledger-host-escape");

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

    // When: the operator moves the held partition on through the handle `host`
    // handed back, on the host that is still running.
    host.delivery()
        .advance_offset(0, held)
        .await
        .expect("the handle a hosted saga hands back reaches the partitions it started");

    // Then
    assert_eq!(
        wait_until_operations(&ledger, 1, "the partition must resume after the escape").await,
        vec!["op-behind".to_owned()],
        "the operator entry on the handle names the running partitions of the \
         host this one declaration started: what lay behind the hold is \
         delivered without the host being restarted"
    );
}
