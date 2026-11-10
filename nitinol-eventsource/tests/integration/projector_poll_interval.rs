//! How often a projector asks its journal is set where the projector is wired,
//! and its first read does not wait for that interval.
//!
//! ```gherkin
//!   Scenario: a deployment chooses how often a projector polls
//!     Given a projector wired with a 1 ms poll interval, spawned either on a
//!           system or from the system's own props
//!     And a stream already holding 1000 records when it starts
//!     When it catches up from that aggregate
//!     Then its read model has counted all of them well inside the default
//!          interval
//!
//!   Scenario: the first read does not wait for the interval
//!     Given a projector wired with an interval far longer than the default
//!     And a stream already holding records when it starts
//!     When it starts
//!     Then those records reach the read model well inside that interval
//!     And a record appended afterwards arrives about one interval after the
//!         first read
//!
//!   Scenario: consecutive reads keep the interval between them
//!     Given a projector wired with an interval I, over a journal that records
//!           when it was asked
//!     When it runs for several reads
//!     Then every gap between consecutive reads is at least I - 50 ms
//!
//!   Scenario: a projector that names no interval keeps the framework's
//!     Given a projector wired with no interval
//!     When it runs for several reads
//!     Then every gap between consecutive reads is the framework's default
//! ```
//!
//! The first scenario alone cannot tell a named interval from an ignored one:
//! one read takes in the whole stream, so a projector polling at the default
//! catches up inside the loose bound too. What says the value reaches the
//! poller is the third scenario, whose interval is far enough above the
//! default that a poller still running at the default leaves gaps too short to
//! pass; it is run through both entry points for that reason. What says the
//! first read does not wait is the second, whose interval is long enough that
//! waiting it out cannot pass for not waiting.
//!
//! The tests run on paused time, so intervals, bounds and the moments the
//! journal records are all measured in the runtime's virtual time. Virtual
//! time advances only when the runtime is idle, never because the machine is
//! busy, so load cannot push a first read that did not wait past "well
//! inside", and a first read that waited for the interval comes exactly one
//! interval after the start. [`SLACK`] and [`LATE_ALLOWANCE`] are margins in
//! that virtual time, not room for a loaded machine.
//!
//! Run with:
//! `cargo test -p nitinol-eventsource --test integration projector_poll_interval::`.

use super::recording_journal;
use recording_journal::RecordingJournal;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use tokio::sync::Notify;
use tokio::time::Instant;

use nitinol_eventsource::codec::Codec;
use nitinol_eventsource::system::EventSourceSystem;
use nitinol_eventsource::{
    Event, ProjectionContext, ProjectionId, Projector, ProjectorProps, TxProvider,
    DEFAULT_POLL_INTERVAL,
};
use nitinol_persistence::provider::{CustomStore, PersistenceProvider, Unprovided};
use nitinol_persistence::store::{EventStore, InMemoryOffsetStore};
use nitinol_persistence::{AggregateId, AppendingEvent, EventType, Family, TypeName};
use nitinol_runtime::ProcessSystem;

/// Short enough that a projector polling at it is never the thing waited on.
const FAST: Duration = Duration::from_millis(1);

/// The stream length the catch-up scenario starts over.
const SEEDED: u64 = 1_000;

/// Far longer than the default, so a first read that waited for it cannot
/// pass for one that did not.
const LONG: Duration = Duration::from_secs(2);

/// Far shorter than [`LONG`], in virtual time: it needs no margin for a loaded
/// machine, only to tell a first read that waited for the interval from one
/// that did not.
const WELL_INSIDE: Duration = Duration::from_millis(500);

/// Far enough above the default that a poller still running at the default
/// leaves gaps that cannot pass for this one.
const SPACED: Duration = Duration::from_millis(600);

/// How much shorter than its interval a gap may read: a timer wakes no earlier
/// than asked, and the two instants are taken on either side of the store
/// call.
const SLACK: Duration = Duration::from_millis(50);

/// How late after one interval a record appended mid-run may still arrive and
/// count as "about one interval".
const LATE_ALLOWANCE: Duration = Duration::from_millis(1_500);

/// How long, in virtual time, any wait below gives the projector before it is
/// a failure.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

// Fixtures: the followed event

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
        Family::new("projector.poll.interval"),
        TypeName::new("Incremented"),
    );

    type Codec = EventCodec;
}

// Fixtures: the read model

/// How many records a projection has applied, and a signal for each one.
#[derive(Clone, Default)]
struct ReadModel {
    applied: Arc<AtomicU64>,
    reached: Arc<Notify>,
}

impl ReadModel {
    fn projector(&self) -> CountingProjector {
        CountingProjector {
            model: self.clone(),
        }
    }

    /// Wait until the read model has counted `expected` records.
    async fn counted(&self, expected: u64) {
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
                    "the read model counted {} records in {PROBE_TIMEOUT:?}, wanted {expected}",
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

// Fixtures: the system

/// The recording journal, plus the projection slot a projector records its
/// position in.
type RecordedStore = CustomStore<Unprovided, InMemoryOffsetStore, Unprovided>;

async fn system_over(journal: &RecordingJournal) -> EventSourceSystem<RecordedStore> {
    EventSourceSystem::builder(ProcessSystem::new().await)
        .with_persistence(PersistenceProvider::new(
            CustomStore::builder()
                .eventstore(journal.clone())
                .projection_offsets(InMemoryOffsetStore::default())
                .build(),
        ))
        .build()
}

/// Append the records numbered `from..=through` to `stream`.
async fn append(journal: &RecordingJournal, stream: &AggregateId, from: u64, through: u64) {
    let events = (from..=through)
        .map(|sequence| AppendingEvent {
            sequence,
            event_type: Incremented::EVENT_TYPE,
            payload: Bytes::new(),
            occurred_at: jiff::Timestamp::now(),
        })
        .collect();
    journal
        .append(stream.as_str(), events)
        .await
        .expect("appending the next sequences of a stream must succeed");
}

// Scenario: a deployment chooses how often a projector polls

#[tokio::test(start_paused = true)]
async fn a_short_interval_reaches_a_projector_spawned_on_a_system() {
    // Given
    let journal = RecordingJournal::default();
    let system = system_over(&journal).await;
    let stream = AggregateId::new("projector-poll-interval-catchup-unhosted");
    append(&journal, &stream, 1, SEEDED).await;
    let model = ReadModel::default();
    let projector_model = model.clone();

    // When
    let started = Instant::now();
    let _projector = ProjectorProps::new(
        ProjectionId::new("projector-poll-interval-catchup-unhosted"),
        move || projector_model.projector(),
    )
    .with_event::<Incremented>()
    .catchup_from_aggregate(stream.clone())
    .with_poll_interval(FAST)
    .spawn(&system)
    .await;
    model.counted(SEEDED).await;
    let caught_up_in = started.elapsed();

    // Then
    assert!(
        caught_up_in < WELL_INSIDE,
        "a projector wired with {FAST:?} took {caught_up_in:?} to count {SEEDED} \
         records; it is waiting out something the interval it was given does \
         not account for"
    );
}

#[tokio::test(start_paused = true)]
async fn a_short_interval_reaches_a_projector_from_the_systems_props() {
    // Given
    let journal = RecordingJournal::default();
    let system = system_over(&journal).await;
    let stream = AggregateId::new("projector-poll-interval-catchup-hosted");
    append(&journal, &stream, 1, SEEDED).await;
    let model = ReadModel::default();
    let projector_model = model.clone();

    // When
    let started = Instant::now();
    let _projector = system
        .projector_props(
            ProjectionId::new("projector-poll-interval-catchup-hosted"),
            move || projector_model.projector(),
        )
        .with_poll_interval(FAST)
        .with_event::<Incremented>()
        .catchup_from_aggregate(stream.clone())
        .spawn()
        .await;
    model.counted(SEEDED).await;
    let caught_up_in = started.elapsed();

    // Then
    assert!(
        caught_up_in < WELL_INSIDE,
        "a projector from the system's props wired with {FAST:?} took \
         {caught_up_in:?} to count {SEEDED} records; it is waiting out \
         something the interval it was given does not account for"
    );
}

// Scenario: the first read does not wait for the interval

#[tokio::test(start_paused = true)]
async fn the_first_read_comes_at_once_and_the_next_one_an_interval_later() {
    // Given
    const ALREADY_THERE: u64 = 3;
    let journal = RecordingJournal::default();
    let system = system_over(&journal).await;
    let stream = AggregateId::new("projector-poll-interval-first-read");
    append(&journal, &stream, 1, ALREADY_THERE).await;
    let model = ReadModel::default();
    let projector_model = model.clone();

    // When: the projector starts over records already in the stream
    let started = Instant::now();
    let _projector = ProjectorProps::new(
        ProjectionId::new("projector-poll-interval-first-read"),
        move || projector_model.projector(),
    )
    .with_poll_interval(LONG)
    .with_event::<Incremented>()
    .catchup_from_aggregate(stream.clone())
    .spawn(&system)
    .await;
    model.counted(ALREADY_THERE).await;
    let first_delivery_in = started.elapsed();

    // Then: they arrive without the interval being waited out first
    assert!(
        first_delivery_in < WELL_INSIDE,
        "the {ALREADY_THERE} records already in the stream took \
         {first_delivery_in:?} to arrive at a projector polling every {LONG:?}; \
         its first read waited for the interval"
    );

    // When: one more record is appended to the same stream, under the same
    // projector
    let first_read = *journal
        .loads_of(stream.as_str())
        .first()
        .expect("records reached the read model, so the stream was read");
    append(&journal, &stream, ALREADY_THERE + 1, ALREADY_THERE + 1).await;
    model.counted(ALREADY_THERE + 1).await;
    let after_first_read = first_read.elapsed();

    // Then: it comes with the next read, an interval after the first
    assert!(
        after_first_read >= LONG - SLACK,
        "the appended record arrived {after_first_read:?} after the first read, \
         sooner than the {LONG:?} the projector was wired with: the reads after \
         the first are not waiting for the interval"
    );
    assert!(
        after_first_read <= LONG + LATE_ALLOWANCE,
        "the appended record arrived {after_first_read:?} after the first read, \
         far later than the one {LONG:?} interval the next read should come at"
    );
}

// Scenario: consecutive reads keep the interval between them

/// Assert every gap between consecutive reads of `stream` is at least
/// `interval`, less the slack, once there have been `reads` of them.
async fn assert_reads_spaced_by(
    journal: &RecordingJournal,
    stream: &AggregateId,
    reads: usize,
    interval: Duration,
    wiring: &str,
) {
    journal
        .wait_until_loads_of(stream.as_str(), reads, PROBE_TIMEOUT)
        .await;
    let gaps = journal.gaps_of(stream.as_str());
    for (nth, gap) in gaps.iter().enumerate() {
        assert!(
            *gap >= interval - SLACK,
            "{wiring}: gap {nth} between reads was {gap:?}, shorter than the \
             {interval:?} it was wired with (all gaps: {gaps:?})"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn reads_of_a_projector_spawned_on_a_system_are_an_interval_apart() {
    // Given
    let journal = RecordingJournal::default();
    let system = system_over(&journal).await;
    let stream = AggregateId::new("projector-poll-interval-gaps-unhosted");
    let model = ReadModel::default();
    let projector_model = model.clone();

    // When: the interval is named before any other builder step, so every
    // step after it has to carry it to the spawn
    let _projector = ProjectorProps::new(
        ProjectionId::new("projector-poll-interval-gaps-unhosted"),
        move || projector_model.projector(),
    )
    .with_poll_interval(SPACED)
    .with_event::<Incremented>()
    .catchup_from_aggregate(stream.clone())
    .spawn(&system)
    .await;

    // Then
    assert_reads_spaced_by(&journal, &stream, 4, SPACED, "spawned on a system").await;
}

#[tokio::test(start_paused = true)]
async fn reads_of_a_projector_from_the_systems_props_are_an_interval_apart() {
    // Given
    let journal = RecordingJournal::default();
    let system = system_over(&journal).await;
    let stream = AggregateId::new("projector-poll-interval-gaps-hosted");
    let model = ReadModel::default();
    let projector_model = model.clone();

    // When: the interval is named last, from the complete builder
    let _projector = system
        .projector_props(
            ProjectionId::new("projector-poll-interval-gaps-hosted"),
            move || projector_model.projector(),
        )
        .with_event::<Incremented>()
        .catchup_from_aggregate(stream.clone())
        .with_poll_interval(SPACED)
        .spawn()
        .await;

    // Then
    assert_reads_spaced_by(&journal, &stream, 4, SPACED, "from the system's props").await;
}

// Scenario: a projector that names no interval keeps the framework's

#[tokio::test(start_paused = true)]
async fn a_projector_that_names_no_interval_reads_at_the_framework_default() {
    // Given
    let journal = RecordingJournal::default();
    let system = system_over(&journal).await;
    let stream = AggregateId::new("projector-poll-interval-default");
    let model = ReadModel::default();
    let projector_model = model.clone();

    // When
    let _projector = system
        .projector_props(
            ProjectionId::new("projector-poll-interval-default"),
            move || projector_model.projector(),
        )
        .with_event::<Incremented>()
        .catchup_from_aggregate(stream.clone())
        .spawn()
        .await;

    // Then: no shorter than the default
    assert_reads_spaced_by(
        &journal,
        &stream,
        3,
        DEFAULT_POLL_INTERVAL,
        "naming no interval",
    )
    .await;

    // And: not some longer cadence of its own either. The bound is loose, so
    // only a default that is not the framework's one fails it.
    let gaps = journal.gaps_of(stream.as_str());
    let loose_ceiling = DEFAULT_POLL_INTERVAL * 4;
    for (nth, gap) in gaps.iter().enumerate() {
        assert!(
            *gap < loose_ceiling,
            "naming no interval: gap {nth} between reads was {gap:?}, far longer \
             than the framework's {DEFAULT_POLL_INTERVAL:?} (all gaps: {gaps:?})"
        );
    }
}

// The interval can be named whatever else the builder holds

/// A provider whose transaction is a type of its own, so props that hold it
/// are in a state no other test here reaches.
struct OwnTxProvider;

struct OwnTx;

#[async_trait]
impl TxProvider for OwnTxProvider {
    type Tx = OwnTx;
    type Error = std::convert::Infallible;

    async fn begin(&self) -> Result<OwnTx, Self::Error> {
        Ok(OwnTx)
    }

    async fn commit(&self, _tx: OwnTx) -> Result<(), Self::Error> {
        Ok(())
    }

    async fn rollback(&self, _tx: OwnTx) {}
}

struct TxReadModel;

#[async_trait]
impl Projector<Incremented, OwnTx> for TxReadModel {
    type Error = std::convert::Infallible;

    async fn project(
        &mut self,
        _event: Incremented,
        _ctx: &mut ProjectionContext<'_, OwnTx>,
    ) -> Result<(), Self::Error> {
        Ok(())
    }
}

/// Compiling is the whole of this one: the interval is a builder step like the
/// others, so props holding a transaction provider take it before and after
/// their event and origin are named, the same as props that hold none.
#[test]
fn the_interval_can_be_named_on_props_that_hold_a_transaction_provider() {
    let _before_the_rest = ProjectorProps::new(ProjectionId::new("tx-before"), || TxReadModel)
        .with_tx_provider(OwnTxProvider)
        .with_poll_interval(SPACED)
        .with_event::<Incremented>()
        .catchup_from_global();
    let _after_the_rest = ProjectorProps::new(ProjectionId::new("tx-after"), || TxReadModel)
        .with_tx_provider(OwnTxProvider)
        .with_event::<Incremented>()
        .catchup_from_global()
        .with_poll_interval(SPACED);
}
