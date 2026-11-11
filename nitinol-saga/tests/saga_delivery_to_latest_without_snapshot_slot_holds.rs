//! A delivery to a `Recover = Latest` aggregate on a deployment without a
//! snapshot slot is a permanent hold, said out loud, and an operator can move
//! past it.
//!
//! ```gherkin
//!   Scenario: the deployment holds no snapshot slot
//!     Given a saga hosted on a system over a store holding a journal and saga
//!           offsets, and no snapshot slot
//!     When the host delivers a decision addressed to a `Latest` aggregate
//!     Then the partition is held at that record, with no progress recorded at
//!          or past it
//!     And a record naming the hold as permanent, the target and the position
//!          is emitted at error level
//!
//!   Scenario: an operator moves the held partition on
//!     Given that held partition, and a decision behind it for a `Never`
//!           aggregate
//!     When the operator advances the partition past the held record
//!     Then the decision behind it is delivered and becomes progress
//!     And nothing is recorded for the held target
//! ```
//!
//! The typed system turns this wiring mistake into a compile error. The host
//! reaches its targets through the contingent system, where no type can say it, so
//! the delivery has to come back as a verdict: retrying would hammer an
//! activation that cannot exist, and skipping would drop the delivery without
//! anyone deciding to. A permanent hold is the answer the host already has for
//! a verdict, and the way out of one is the operator's.
//!
//! The capture is the global `tracing::Subscriber` used by
//! `saga_delivery_transient_failure_retries.rs`, which is why this file is a
//! test binary of its own.

#[path = "common/delivery.rs"]
mod delivery;
use delivery::{
    append_decided, delivery_system, operations_of, scope_of, settle, spawn_hosted, stream_rows,
    wait_until_loads, wait_until_operations, wait_until_stored, Ledger, RecordingEventStore,
    RecordingOffsetStore, ShelvingSaga, SHELF_SUFFIX, TEST_DELIVERY_ID,
};

use std::collections::BTreeMap;
use std::fmt::Debug;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use nitinol_persistence::store::InMemoryEventStore;
use nitinol_persistence::AggregateId;
use nitinol_saga::SagaId;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Level, Metadata, Subscriber};

/// The fields a record about a held delivery has to carry.
const RETRYABILITY: &str = "retryability";
const TARGET: &str = "target";
const GLOBAL_SEQUENCE: &str = "global_sequence";
const PARTITION: &str = "partition";
const DELIVERY_ID: &str = "delivery_id";

const CAPTURE_LOCK: &str = "the capture buffer guards only a push and a read, so no holder panics";

type Fields = BTreeMap<String, String>;

#[derive(Default)]
struct FieldMap(Fields);

impl Visit for FieldMap {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name().to_owned(), value.to_owned());
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.0.insert(field.name().to_owned(), value.to_string());
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.0.insert(field.name().to_owned(), value.to_string());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn Debug) {
        let rendered = format!("{value:?}");
        let unquoted = rendered
            .strip_prefix('"')
            .and_then(|rest| rest.strip_suffix('"'))
            .unwrap_or(&rendered)
            .to_owned();
        self.0.insert(field.name().to_owned(), unquoted);
    }
}

/// One captured record: its fields, and the level it was emitted at.
#[derive(Clone)]
struct Captured {
    fields: Fields,
    level: Level,
}

struct Recorder {
    captured: Arc<Mutex<Vec<Captured>>>,
}

impl Subscriber for Recorder {
    fn enabled(&self, _metadata: &Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _span: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }

    fn record(&self, _span: &Id, _values: &Record<'_>) {}

    fn record_follows_from(&self, _span: &Id, _follows: &Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        let mut fields = FieldMap::default();
        event.record(&mut fields);
        self.captured.lock().expect(CAPTURE_LOCK).push(Captured {
            fields: fields.0,
            level: *event.metadata().level(),
        });
    }

    fn enter(&self, _span: &Id) {}

    fn exit(&self, _span: &Id) {}
}

#[derive(Clone)]
struct Capture(Arc<Mutex<Vec<Captured>>>);

static CAPTURE: OnceLock<Capture> = OnceLock::new();

impl Capture {
    /// The one process-wide capture. A process has a single global subscriber,
    /// so every test in this binary reads the same buffer and selects its own
    /// records by the target they name.
    fn installed() -> Self {
        CAPTURE
            .get_or_init(|| {
                let captured = Arc::new(Mutex::new(Vec::new()));
                tracing::subscriber::set_global_default(Recorder {
                    captured: Arc::clone(&captured),
                })
                .expect("this binary installs the capture once, before anything else emits");
                Capture(captured)
            })
            .clone()
    }

    /// Records carrying every required field and naming `target`.
    fn hold_records(&self, target: &str) -> Vec<Captured> {
        self.0
            .lock()
            .expect(CAPTURE_LOCK)
            .iter()
            .filter(|captured| {
                [
                    RETRYABILITY,
                    TARGET,
                    GLOBAL_SEQUENCE,
                    PARTITION,
                    DELIVERY_ID,
                ]
                .iter()
                .all(|name| captured.fields.contains_key(*name))
                    && captured.fields.get(TARGET).map(String::as_str) == Some(target)
            })
            .cloned()
            .collect()
    }

    async fn wait_for_hold_record(&self, target: &str) -> Captured {
        let deadline = Instant::now() + delivery::PROBE_TIMEOUT;
        loop {
            if let Some(first) = self.hold_records(target).into_iter().next() {
                return first;
            }
            if Instant::now() >= deadline {
                panic!("no record diagnosing a held delivery to {target} was emitted");
            }
            tokio::time::sleep(delivery::PROBE_INTERVAL).await;
        }
    }
}

#[tokio::test]
async fn a_delivery_to_a_latest_aggregate_without_a_snapshot_slot_is_a_reported_permanent_hold() {
    let capture = Capture::installed();

    // Given: a deployment holding a journal and saga offsets, and no snapshot
    // slot
    let recording = RecordingEventStore::new(InMemoryEventStore::default());
    let offsets = RecordingOffsetStore::new();
    let system = delivery_system(&recording, &offsets).await;
    let saga_id = SagaId::new("instance-1");
    let target = format!("no-slot-held{SHELF_SUFFIX}");
    let held = append_decided(&recording, &saga_id, 1, &target, "op-held").await;

    // When
    let _host = spawn_hosted::<ShelvingSaga, _>(&system, 1).await;

    // Then: the hold is reported as permanent
    let record = capture.wait_for_hold_record(&target).await;
    assert_eq!(
        record.fields.get(RETRYABILITY).map(String::as_str),
        Some("Permanent"),
        "a deployment without a snapshot slot will not grow one, so the hold is \
         a verdict to act on rather than a failure to wait out"
    );
    assert_eq!(
        record.level,
        Level::ERROR,
        "a permanent hold is emitted at the level an operator acts on"
    );
    let held_text = held.to_string();
    assert_eq!(
        record.fields.get(GLOBAL_SEQUENCE).map(String::as_str),
        Some(held_text.as_str()),
        "the record names the position the partition is held at"
    );
    assert_eq!(
        record.fields.get(PARTITION).map(String::as_str),
        Some("0"),
        "the record names the held partition"
    );
    assert_eq!(
        record.fields.get(DELIVERY_ID).map(String::as_str),
        Some(TEST_DELIVERY_ID),
        "the record names which host is held"
    );

    // And: the partition is held at that record
    wait_until_loads(&recording, 3, "the host must keep polling while held").await;
    settle().await;
    assert!(
        !offsets
            .save_calls()
            .iter()
            .any(|(_, sequence)| *sequence >= held),
        "a delivery that never reached its target is not progress, so no \
         position at or past it may be recorded"
    );
    assert!(
        stream_rows(&recording, &target).await.is_empty(),
        "nothing is recorded for a target that could not be activated"
    );
}

#[tokio::test]
async fn an_operator_moves_a_partition_held_on_a_latest_aggregate_past_the_hold() {
    // Installed here as well, so the order the two tests run in does not decide
    // which dispatcher the host's records go to.
    let _capture = Capture::installed();

    // Given: a partition held on a `Latest` target, and a decision behind it
    // for a `Never` one
    let recording = RecordingEventStore::new(InMemoryEventStore::default());
    let offsets = RecordingOffsetStore::new();
    let system = delivery_system(&recording, &offsets).await;
    let saga_id = SagaId::new("instance-1");
    let held_target = format!("no-slot-escape{SHELF_SUFFIX}");
    let live_target = AggregateId::new("ledger-behind-the-latest-hold");
    let held = append_decided(&recording, &saga_id, 1, &held_target, "op-held").await;
    let behind = append_decided(&recording, &saga_id, 2, live_target.as_str(), "op-behind").await;

    let host = spawn_hosted::<ShelvingSaga, _>(&system, 1).await;
    wait_until_loads(&recording, 3, "the host must reach the held record").await;
    settle().await;

    let ledger = system.aggregate_proxy::<Ledger>(live_target);
    assert!(
        operations_of(&ledger).await.is_empty(),
        "before the escape, the decision behind the hold is not delivered"
    );

    // When
    host.advance_offset(0, held)
        .await
        .expect("moving a held partition forward is the operator's way out");

    // Then
    assert_eq!(
        wait_until_operations(&ledger, 1, "the partition must resume after the escape").await,
        vec!["op-behind".to_owned()],
        "the partition delivers what lay behind the hold"
    );
    assert_eq!(
        wait_until_stored(
            &offsets,
            &scope_of(0),
            behind,
            "progress must move on from the new position",
        )
        .await,
        behind,
    );
    assert!(
        stream_rows(&recording, &held_target).await.is_empty(),
        "the delivery that was passed over is not made"
    );
}
