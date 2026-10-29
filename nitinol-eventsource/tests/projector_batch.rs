//! A projector settles what one read of its journal returned as one unit.
//!
//! ```gherkin
//!   Scenario: ExactlyOnce commits a batch once
//!     Given a stream holding 5 records and an ExactlyOnce projector with a
//!           TxProvider
//!     When it catches up in one read
//!     Then it begins one transaction, hands that transaction to every
//!          handler, saves the offset at 5 inside it once, and commits once
//!
//!   Scenario: a failure inside the batch takes the whole batch back
//!     Given the same projector, whose handler fails for record 3
//!     Then each attempt rolls back, nothing is saved or committed, and the
//!          next attempt starts again from record 1
//!     And no record of the batch reaches the read model
//!
//!   Scenario: AtLeastOnce saves up to the last success
//!     Given an AtLeastOnce projector whose handler fails record 3 once
//!     Then the offset is saved once at 2, and redelivery starts at 3
//!
//!   Scenario: AtMostOnce saves the end of the batch first
//!     Given an AtMostOnce projector whose handler always fails record 3
//!     Then the offset is saved once at 5 before any handler runs, every
//!          other record is applied, and nothing is redelivered
//!
//!   Scenario: ExactlyOnce without a TxProvider stops at the failure
//!     Given an ExactlyOnce projector with no TxProvider, failing record 3 once
//!     Then the framework saves nothing, and redelivery starts at 3
//!
//!   Scenario: a batch that is all skips does no work
//!     Given an ExactlyOnce projector with a TxProvider, over a stream whose
//!           first records are of a type it does not follow
//!     Then reading them opens no transaction and saves no offset
//! ```
//!
//! Every assertion is made against one timeline the transaction provider, the
//! offset store and the handlers all write to (`common/batch_ledger.rs`), so
//! the order of begin, handler, save and commit is observed rather than
//! inferred from counts.
//!
//! Run with: `cargo test -p nitinol-eventsource --test projector_batch`.

#[path = "common/recording_journal.rs"]
mod recording_journal;
use recording_journal::RecordingJournal;

#[path = "common/batch_ledger.rs"]
mod batch_ledger;
use batch_ledger::{
    append_range, Evt, Failure, Ledger, LedgerOffsetStore, LedgerTxProvider, MockTx,
    PlainReadModel, Step, TxReadModel, PERMANENT, UNREGISTERED_EVENT_TYPE, WAIT_TIMEOUT,
};

use nitinol_eventsource::system::EventSourceSystem;
use nitinol_eventsource::{Event, ProjectionId, ProjectorProps};
use nitinol_persistence::provider::{CustomStore, PersistenceProvider, Unprovided};
use nitinol_persistence::store::{DeliveryMode, OffsetScope, OffsetStore};
use nitinol_persistence::AggregateId;
use nitinol_runtime::ProcessSystem;

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

fn applied(sequence: u64, tx: Option<u64>) -> Step {
    Step::Project {
        sequence,
        tx,
        applied: true,
    }
}

fn failed(sequence: u64, tx: Option<u64>) -> Step {
    Step::Project {
        sequence,
        tx,
        applied: false,
    }
}

/// The steps up to and including the `n`th rollback, or all of them if there
/// were fewer.
fn through_nth_rollback(steps: &[Step], n: usize) -> Vec<Step> {
    let mut seen = 0;
    let end = steps
        .iter()
        .position(|step| {
            if matches!(step, Step::Rollback { .. }) {
                seen += 1;
            }
            seen == n
        })
        .map_or(steps.len(), |at| at + 1);
    steps[..end].to_vec()
}

// Scenario: ExactlyOnce commits a batch once

#[tokio::test(flavor = "multi_thread")]
async fn exactly_once_applies_a_caught_up_batch_in_one_transaction() {
    // Given
    let journal = RecordingJournal::default();
    let ledger = Ledger::default();
    let system = system_over::<MockTx>(&journal, &ledger).await;
    let stream = AggregateId::new("batch-exactly-once-one-tx");
    let projection_id = ProjectionId::new("batch-exactly-once-one-tx");
    let scope = OffsetScope::from(&projection_id);
    append_range(&journal, &stream, 1, 5, Evt::EVENT_TYPE).await;

    // When
    let read_model = ledger.clone();
    let _projector = ProjectorProps::new(projection_id, move || TxReadModel {
        ledger: read_model.clone(),
        failure: Failure::none(),
    })
    .with_tx_provider(LedgerTxProvider::new(&ledger))
    .with_event::<Evt>()
    .catchup_from_aggregate(stream)
    .spawn(&system)
    .await;
    let settled = ledger.wait_for(|l| l.offset_of(&scope) == Some(5)).await;

    // Then
    let steps = ledger.steps();
    assert!(
        settled,
        "the offset never reached 5 in {WAIT_TIMEOUT:?}; steps {steps:?}"
    );
    assert_eq!(
        steps,
        vec![
            Step::Begin { tx: 1 },
            applied(1, Some(1)),
            applied(2, Some(1)),
            applied(3, Some(1)),
            applied(4, Some(1)),
            applied(5, Some(1)),
            Step::Save {
                sequence: 5,
                tx: Some(1)
            },
            Step::Commit { tx: 1 },
        ],
        "one read of 5 records must be one transaction: one begin, every handler \
         inside it, the offset saved once at the batch's end inside it, one commit"
    );
    assert_eq!(
        ledger.read_model(),
        vec![1, 2, 3, 4, 5],
        "the committed transaction must carry every record of the batch"
    );
}

// Scenario: a failure inside the batch takes the whole batch back

#[tokio::test(flavor = "multi_thread")]
async fn exactly_once_failure_inside_a_batch_rolls_the_whole_batch_back() {
    // Given
    let journal = RecordingJournal::default();
    let ledger = Ledger::default();
    let system = system_over::<MockTx>(&journal, &ledger).await;
    let stream = AggregateId::new("batch-exactly-once-rollback");
    let projection_id = ProjectionId::new("batch-exactly-once-rollback");
    let scope = OffsetScope::from(&projection_id);
    append_range(&journal, &stream, 1, 5, Evt::EVENT_TYPE).await;

    // When
    let read_model = ledger.clone();
    let failure = Failure::at(3, PERMANENT);
    let _projector = ProjectorProps::new(projection_id, move || TxReadModel {
        ledger: read_model.clone(),
        failure: failure.clone(),
    })
    .with_tx_provider(LedgerTxProvider::new(&ledger))
    .with_event::<Evt>()
    .catchup_from_aggregate(stream)
    .spawn(&system)
    .await;
    let retried = ledger.wait_for(|l| l.rollbacks() >= 2).await;

    // Then
    let steps = ledger.steps();
    assert!(
        retried,
        "the batch was not attempted twice in {WAIT_TIMEOUT:?}; steps {steps:?}"
    );
    assert_eq!(
        through_nth_rollback(&steps, 2),
        vec![
            Step::Begin { tx: 1 },
            applied(1, Some(1)),
            applied(2, Some(1)),
            failed(3, Some(1)),
            Step::Rollback { tx: 1 },
            Step::Begin { tx: 2 },
            applied(1, Some(2)),
            applied(2, Some(2)),
            failed(3, Some(2)),
            Step::Rollback { tx: 2 },
        ],
        "a failing record must roll the whole batch's transaction back, stop the \
         batch there, and have the next attempt start again from the batch's first \
         record"
    );
    assert!(
        steps
            .iter()
            .all(|s| !matches!(s, Step::Save { .. } | Step::Commit { .. })),
        "nothing may be saved or committed while a record of the batch fails; \
         steps {steps:?}"
    );
    assert_eq!(
        ledger.read_model(),
        Vec::<u64>::new(),
        "no record of a rolled-back batch may reach the read model"
    );
    assert_eq!(
        ledger.offset_of(&scope),
        None,
        "the offset must stay where it was before the batch"
    );
}

// Scenario: AtLeastOnce saves up to the last success

#[tokio::test(flavor = "multi_thread")]
async fn at_least_once_saves_through_the_last_success_and_resumes_at_the_failure() {
    // Given
    let journal = RecordingJournal::default();
    let ledger = Ledger::default();
    let system = system_over::<()>(&journal, &ledger).await;
    let stream = AggregateId::new("batch-at-least-once-partial");
    let projection_id = ProjectionId::new("batch-at-least-once-partial");
    let scope = OffsetScope::from(&projection_id);
    append_range(&journal, &stream, 1, 5, Evt::EVENT_TYPE).await;

    // When
    let read_model = ledger.clone();
    let failure = Failure::at(3, 1);
    let _projector = ProjectorProps::new(projection_id, move || PlainReadModel {
        ledger: read_model.clone(),
        failure: failure.clone(),
    })
    .with_event::<Evt>()
    .delivery_mode(DeliveryMode::AtLeastOnce)
    .catchup_from_aggregate(stream)
    .spawn(&system)
    .await;
    let settled = ledger.wait_for(|l| l.offset_of(&scope) == Some(5)).await;

    // Then
    let steps = ledger.steps();
    assert!(
        settled,
        "the offset never reached 5 in {WAIT_TIMEOUT:?}; steps {steps:?}"
    );
    assert_eq!(
        steps,
        vec![
            applied(1, None),
            applied(2, None),
            failed(3, None),
            Step::Save {
                sequence: 2,
                tx: None
            },
            applied(3, None),
            applied(4, None),
            applied(5, None),
            Step::Save {
                sequence: 5,
                tx: None
            },
        ],
        "a batch that fails at record 3 must save once at 2, stop there, and be \
         redelivered from 3; the redelivered batch saves once at its end"
    );
}

// Scenario: AtMostOnce saves the end of the batch first

#[tokio::test(flavor = "multi_thread")]
async fn at_most_once_saves_the_batch_end_before_projecting_and_never_redelivers() {
    // Given
    let journal = RecordingJournal::default();
    let ledger = Ledger::default();
    let system = system_over::<()>(&journal, &ledger).await;
    let stream = AggregateId::new("batch-at-most-once");
    let projection_id = ProjectionId::new("batch-at-most-once");
    let scope = OffsetScope::from(&projection_id);
    append_range(&journal, &stream, 1, 5, Evt::EVENT_TYPE).await;

    // When
    let read_model = ledger.clone();
    let failure = Failure::at(3, PERMANENT);
    let _projector = ProjectorProps::new(projection_id, move || PlainReadModel {
        ledger: read_model.clone(),
        failure: failure.clone(),
    })
    .with_event::<Evt>()
    .delivery_mode(DeliveryMode::AtMostOnce)
    .catchup_from_aggregate(stream.clone())
    .spawn(&system)
    .await;
    // A third read only starts once the second one's delivery has returned, so
    // anything the second read redelivered is on the timeline by then.
    journal
        .wait_until_loads_of(stream.as_str(), 3, WAIT_TIMEOUT)
        .await;

    // Then
    assert_eq!(
        ledger.steps(),
        vec![
            Step::Save {
                sequence: 5,
                tx: None
            },
            applied(1, None),
            applied(2, None),
            failed(3, None),
            applied(4, None),
            applied(5, None),
        ],
        "AtMostOnce must save the batch's end once before any handler runs, go \
         past the failing record to the rest of the batch, and redeliver nothing"
    );
    assert_eq!(ledger.offset_of(&scope), Some(5));
}

// Scenario: ExactlyOnce without a TxProvider stops at the failure

#[tokio::test(flavor = "multi_thread")]
async fn exactly_once_without_tx_stops_at_the_failure_and_resumes_there() {
    // Given
    let journal = RecordingJournal::default();
    let ledger = Ledger::default();
    let system = system_over::<()>(&journal, &ledger).await;
    let stream = AggregateId::new("batch-exactly-once-no-tx");
    let projection_id = ProjectionId::new("batch-exactly-once-no-tx");
    append_range(&journal, &stream, 1, 5, Evt::EVENT_TYPE).await;

    // When
    let read_model = ledger.clone();
    let failure = Failure::at(3, 1);
    let _projector = ProjectorProps::new(projection_id, move || PlainReadModel {
        ledger: read_model.clone(),
        failure: failure.clone(),
    })
    .with_event::<Evt>()
    .delivery_mode(DeliveryMode::ExactlyOnce)
    .catchup_from_aggregate(stream)
    .spawn(&system)
    .await;
    let reached_the_end = ledger.wait_for(|l| l.read_model().contains(&5)).await;

    // Then
    let steps = ledger.steps();
    assert!(
        reached_the_end,
        "record 5 was not applied in {WAIT_TIMEOUT:?}; steps {steps:?}"
    );
    assert_eq!(
        steps,
        vec![
            applied(1, None),
            applied(2, None),
            failed(3, None),
            applied(3, None),
            applied(4, None),
            applied(5, None),
        ],
        "without a TxProvider the records before the failure are settled, so \
         redelivery starts at the failing record; and the framework saves no offset"
    );
}

// Scenario: a batch that is all skips does no work

#[tokio::test(flavor = "multi_thread")]
async fn a_batch_of_only_skipped_records_opens_no_transaction() {
    // Given
    let journal = RecordingJournal::default();
    let ledger = Ledger::default();
    let system = system_over::<MockTx>(&journal, &ledger).await;
    let stream = AggregateId::new("batch-all-skipped");
    let projection_id = ProjectionId::new("batch-all-skipped");
    let scope = OffsetScope::from(&projection_id);
    append_range(&journal, &stream, 1, 3, UNREGISTERED_EVENT_TYPE).await;

    // When: the unfollowed records are read and delivered
    let read_model = ledger.clone();
    let _projector = ProjectorProps::new(projection_id, move || TxReadModel {
        ledger: read_model.clone(),
        failure: Failure::none(),
    })
    .with_tx_provider(LedgerTxProvider::new(&ledger))
    .with_event::<Evt>()
    .catchup_from_aggregate(stream.clone())
    .spawn(&system)
    .await;
    // The second read starts only after the first one's delivery returned.
    journal
        .wait_until_loads_of(stream.as_str(), 2, WAIT_TIMEOUT)
        .await;
    let after_the_skips = ledger.steps();

    // And: a followed record comes after them
    append_range(&journal, &stream, 4, 4, Evt::EVENT_TYPE).await;
    let settled = ledger.wait_for(|l| l.offset_of(&scope) == Some(4)).await;

    // Then
    assert_eq!(
        after_the_skips,
        Vec::<Step>::new(),
        "a batch whose every record is skipped must begin no transaction and save \
         no offset"
    );
    let steps = ledger.steps();
    assert!(
        settled,
        "the followed record never settled in {WAIT_TIMEOUT:?}; steps {steps:?}"
    );
    assert_eq!(
        steps,
        vec![
            Step::Begin { tx: 1 },
            applied(4, Some(1)),
            Step::Save {
                sequence: 4,
                tx: Some(1)
            },
            Step::Commit { tx: 1 },
        ],
        "the followed record after the skipped ones is applied in a transaction of \
         its own"
    );
}
