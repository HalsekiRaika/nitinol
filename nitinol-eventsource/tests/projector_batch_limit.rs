//! A projector reads its journal at most a batch limit at a time, and a read
//! that came back full is followed by the next one without waiting for the
//! poll interval.
//!
//! ```gherkin
//!   Scenario: the limit splits a backlog, and a full batch does not wait
//!     Given limit = 10, interval = 2 s, and a stream holding 25 records
//!     When the projector is spawned
//!     Then 25 records are applied in 3 reads, all well inside 500 ms
//!     And with ExactlyOnce + TxProvider that is 3 commits
//!
//!   Scenario: a projector that names no limit reads DEFAULT_BATCH_LIMIT at a time
//!     Given a stream holding DEFAULT_BATCH_LIMIT + 1 records
//!     Then they are applied in 2 batches, the second without waiting
//!
//!   Scenario: a full batch that did not fully settle waits the interval
//!     Given limit = 3 and a full first read whose second record fails once
//!     Then the next read comes an interval later, not at once
//! ```
//!
//! The limit is named at both ends of the builder and through both entry
//! points — first on props that take a system at spawn, last on the system's
//! own props — so every builder step in between has to carry it.
//!
//! "Well inside" is loose on purpose, as in `projector_poll_interval.rs`: an
//! interval far longer than the bound, so scheduling on a loaded machine does
//! not decide the result.
//!
//! Run with: `cargo test -p nitinol-eventsource --test projector_batch_limit`.

#[path = "common/recording_journal.rs"]
mod recording_journal;
use recording_journal::RecordingJournal;

#[path = "common/batch_ledger.rs"]
mod batch_ledger;
use batch_ledger::{
    append_range, Evt, Failure, Ledger, LedgerOffsetStore, LedgerTxProvider, MockTx,
    PlainReadModel, Step, TxReadModel, WAIT_TIMEOUT,
};

use std::num::NonZeroUsize;
use std::time::{Duration, Instant};

use nitinol_eventsource::system::EventSourceSystem;
use nitinol_eventsource::{Event, ProjectionId, ProjectorProps, DEFAULT_BATCH_LIMIT};
use nitinol_persistence::provider::{CustomStore, PersistenceProvider, Unprovided};
use nitinol_persistence::store::{OffsetScope, OffsetStore};
use nitinol_persistence::AggregateId;
use nitinol_runtime::ProcessSystem;

const LIMIT: NonZeroUsize = NonZeroUsize::new(10).expect("10 is not zero");

const SEEDED: u64 = 25;

/// Far longer than [`WELL_INSIDE`], so a read that waited for it cannot pass
/// for one that did not.
const LONG: Duration = Duration::from_secs(2);

const WELL_INSIDE: Duration = Duration::from_millis(500);

/// How much shorter than its interval a gap between reads may be: a timer wakes
/// no earlier than asked, and the two instants are taken around the store call.
const SLACK: Duration = Duration::from_millis(50);

type LedgerStore<Tx> = CustomStore<Unprovided, LedgerOffsetStore<Tx>, Unprovided>;

async fn system_over<Tx>(
    journal: &RecordingJournal,
    ledger: &Ledger,
) -> EventSourceSystem<LedgerStore<Tx>>
where
    LedgerOffsetStore<Tx>: OffsetStore + 'static,
{
    EventSourceSystem::builder(ProcessSystem::new().await)
        .with_persistence(PersistenceProvider::new(
            CustomStore::builder()
                .eventstore(journal.clone())
                .projection_offsets(LedgerOffsetStore::<Tx>::new(ledger))
                .build(),
        ))
        .build()
}

fn saved(sequence: u64, tx: Option<u64>) -> Step {
    Step::Save { sequence, tx }
}

// Scenario: the limit splits a backlog, and a full batch does not wait

#[tokio::test(flavor = "multi_thread")]
async fn a_limit_named_first_splits_a_backlog_into_batches_read_back_to_back() {
    // Given
    let journal = RecordingJournal::default();
    let ledger = Ledger::default();
    let system = system_over::<MockTx>(&journal, &ledger).await;
    let stream = AggregateId::new("batch-limit-unhosted");
    let projection_id = ProjectionId::new("batch-limit-unhosted");
    let scope = OffsetScope::from(&projection_id);
    append_range(&journal, &stream, 1, SEEDED, Evt::EVENT_TYPE).await;

    // When: the limit is named before any other builder step, so every step
    // after it has to carry it to the spawn
    let read_model = ledger.clone();
    let started = Instant::now();
    let _projector = ProjectorProps::new(projection_id, move || TxReadModel {
        ledger: read_model.clone(),
        failure: Failure::none(),
    })
    .with_batch_limit(LIMIT)
    .with_tx_provider(LedgerTxProvider::new(&ledger))
    .with_poll_interval(LONG)
    .with_event::<Evt>()
    .catchup_from_aggregate(stream.clone())
    .spawn(&system)
    .await;
    let settled = ledger
        .wait_for(|l| l.offset_of(&scope) == Some(SEEDED))
        .await;
    let caught_up_in = started.elapsed();

    // Then
    assert!(
        settled,
        "the offset never reached {SEEDED} in {WAIT_TIMEOUT:?}; saves {:?}",
        ledger.saves()
    );
    assert_eq!(
        ledger.saves(),
        vec![saved(10, Some(1)), saved(20, Some(2)), saved(25, Some(3))],
        "a limit of {LIMIT} must cut {SEEDED} records into batches of 10, 10 and 5, \
         each saved once at its end inside a transaction of its own"
    );
    assert_eq!(
        ledger.commits(),
        3,
        "ExactlyOnce + TxProvider commits once per batch"
    );
    assert_eq!(
        journal.loads_of(stream.as_str()).len(),
        3,
        "{SEEDED} records under a limit of {LIMIT} take exactly 3 reads"
    );
    assert!(
        caught_up_in < WELL_INSIDE,
        "{SEEDED} records took {caught_up_in:?} under a {LONG:?} interval: a read \
         that came back full waited for the interval before the next one"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_limit_named_last_on_the_systems_props_splits_a_backlog_the_same_way() {
    // Given
    let journal = RecordingJournal::default();
    let ledger = Ledger::default();
    let system = system_over::<()>(&journal, &ledger).await;
    let stream = AggregateId::new("batch-limit-hosted");
    let projection_id = ProjectionId::new("batch-limit-hosted");
    let scope = OffsetScope::from(&projection_id);
    append_range(&journal, &stream, 1, SEEDED, Evt::EVENT_TYPE).await;

    // When: the limit is named last, from the complete builder, on an
    // AtLeastOnce projector
    let read_model = ledger.clone();
    let started = Instant::now();
    let _projector = system
        .projector_props(projection_id, move || PlainReadModel {
            ledger: read_model.clone(),
            failure: Failure::none(),
        })
        .with_event::<Evt>()
        .catchup_from_aggregate(stream.clone())
        .with_poll_interval(LONG)
        .with_batch_limit(LIMIT)
        .spawn()
        .await;
    let settled = ledger
        .wait_for(|l| l.offset_of(&scope) == Some(SEEDED))
        .await;
    let caught_up_in = started.elapsed();

    // Then
    assert!(
        settled,
        "the offset never reached {SEEDED} in {WAIT_TIMEOUT:?}; saves {:?}",
        ledger.saves()
    );
    assert_eq!(
        ledger.saves(),
        vec![saved(10, None), saved(20, None), saved(25, None)],
        "a limit of {LIMIT} must cut {SEEDED} records into batches of 10, 10 and 5, \
         each saved once at its end"
    );
    assert_eq!(
        journal.loads_of(stream.as_str()).len(),
        3,
        "{SEEDED} records under a limit of {LIMIT} take exactly 3 reads"
    );
    assert!(
        caught_up_in < WELL_INSIDE,
        "{SEEDED} records took {caught_up_in:?} under a {LONG:?} interval: a read \
         that came back full waited for the interval before the next one"
    );
}

// Scenario: a projector that names no limit reads DEFAULT_BATCH_LIMIT at a time

#[tokio::test(flavor = "multi_thread")]
async fn a_projector_that_names_no_limit_reads_the_default_limit_at_a_time() {
    // Given
    assert_eq!(DEFAULT_BATCH_LIMIT.get(), 1_000);
    let limit = u64::try_from(DEFAULT_BATCH_LIMIT.get()).expect("1000 fits in a u64");
    let journal = RecordingJournal::default();
    let ledger = Ledger::default();
    let system = system_over::<MockTx>(&journal, &ledger).await;
    let stream = AggregateId::new("batch-limit-default");
    let projection_id = ProjectionId::new("batch-limit-default");
    let scope = OffsetScope::from(&projection_id);
    append_range(&journal, &stream, 1, limit + 1, Evt::EVENT_TYPE).await;

    // When
    let read_model = ledger.clone();
    let started = Instant::now();
    let _projector = ProjectorProps::new(projection_id, move || TxReadModel {
        ledger: read_model.clone(),
        failure: Failure::none(),
    })
    .with_tx_provider(LedgerTxProvider::new(&ledger))
    .with_poll_interval(LONG)
    .with_event::<Evt>()
    .catchup_from_aggregate(stream)
    .spawn(&system)
    .await;
    let settled = ledger
        .wait_for(|l| l.offset_of(&scope) == Some(limit + 1))
        .await;
    let caught_up_in = started.elapsed();

    // Then
    assert!(
        settled,
        "the offset never reached {} in {WAIT_TIMEOUT:?}; saves {:?}",
        limit + 1,
        ledger.saves()
    );
    assert_eq!(
        ledger.saves(),
        vec![saved(limit, Some(1)), saved(limit + 1, Some(2))],
        "props that name no limit must read {DEFAULT_BATCH_LIMIT} records at a time"
    );
    assert!(
        caught_up_in < WELL_INSIDE,
        "{} records took {caught_up_in:?} under a {LONG:?} interval: the read after \
         a full default-sized batch waited for the interval",
        limit + 1
    );
}

// Scenario: a full batch that did not fully settle waits the interval

#[tokio::test(flavor = "multi_thread")]
async fn a_full_batch_that_did_not_fully_settle_waits_the_interval() {
    // Given
    const INTERVAL: Duration = Duration::from_millis(600);
    let limit = NonZeroUsize::new(3).expect("3 is not zero");
    let journal = RecordingJournal::default();
    let ledger = Ledger::default();
    let system = system_over::<()>(&journal, &ledger).await;
    let stream = AggregateId::new("batch-limit-partial");
    let projection_id = ProjectionId::new("batch-limit-partial");
    append_range(&journal, &stream, 1, 3, Evt::EVENT_TYPE).await;

    // When: the first read is full, and its second record fails once
    let read_model = ledger.clone();
    let failure = Failure::at(2, 1);
    let _projector = ProjectorProps::new(projection_id, move || PlainReadModel {
        ledger: read_model.clone(),
        failure: failure.clone(),
    })
    .with_event::<Evt>()
    .catchup_from_aggregate(stream.clone())
    .with_poll_interval(INTERVAL)
    .with_batch_limit(limit)
    .spawn(&system)
    .await;
    journal
        .wait_until_loads_of(stream.as_str(), 2, WAIT_TIMEOUT)
        .await;

    // Then: the first read was the full, partly settled one. Its delivery had
    // returned before the second read began, so its steps are all recorded.
    let steps = ledger.steps();
    assert_eq!(
        steps.get(..3),
        Some(
            &[
                Step::Project {
                    sequence: 1,
                    tx: None,
                    applied: true
                },
                Step::Project {
                    sequence: 2,
                    tx: None,
                    applied: false
                },
                saved(1, None),
            ][..]
        ),
        "the first read must have been a full batch settled through 1 only; steps \
         {steps:?}"
    );
    // And: the read after it kept the interval
    let gaps = journal.gaps_of(stream.as_str());
    assert!(
        gaps[0] >= INTERVAL - SLACK,
        "the read after a full batch that settled only partly came {:?} after it, \
         sooner than the {INTERVAL:?} interval: only a batch that came back full \
         and settled whole may skip the wait (all gaps: {gaps:?})",
        gaps[0]
    );
}

// The limit can be named whatever else the builder holds

/// Compiling is the whole of this one, as with the poll interval: the limit is
/// a builder step like the others, so props holding a transaction provider
/// take it before and after their event and origin are named.
#[test]
fn the_batch_limit_can_be_named_on_props_that_hold_a_transaction_provider() {
    let ledger = Ledger::default();
    let read_model = || {
        let ledger = ledger.clone();
        move || TxReadModel {
            ledger: ledger.clone(),
            failure: Failure::none(),
        }
    };
    let _before_the_event = ProjectorProps::new(ProjectionId::new("tx-before"), read_model())
        .with_tx_provider(LedgerTxProvider::new(&ledger))
        .with_batch_limit(LIMIT)
        .with_event::<Evt>()
        .catchup_from_global();
    let _between = ProjectorProps::new(ProjectionId::new("tx-between"), read_model())
        .with_tx_provider(LedgerTxProvider::new(&ledger))
        .with_event::<Evt>()
        .with_batch_limit(LIMIT)
        .catchup_from_global();
    let _after_the_rest = ProjectorProps::new(ProjectionId::new("tx-after"), read_model())
        .with_tx_provider(LedgerTxProvider::new(&ledger))
        .with_event::<Evt>()
        .catchup_from_global()
        .with_batch_limit(LIMIT);
}
