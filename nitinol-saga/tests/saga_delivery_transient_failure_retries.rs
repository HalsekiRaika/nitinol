//! A failure of the machinery is retried, with room between the attempts.
//!
//! ```gherkin
//!   Scenario: the receiver cannot record what it accepted
//!     Given a receiver whose store refuses every write
//!     When the host delivers a decision to it
//!     Then the cursor does not move past that record
//!     And the same record is delivered again after a pause
//!     And the pause grows while the failure persists
//!     And a record naming the failure, the target and the position is emitted
//! ```
//!
//! Nothing was decided about the command: the receiver accepted it and the
//! store it writes to failed, so re-issuing may well succeed. That is why the
//! answer is to keep trying rather than to give up — and why the pause has to
//! grow, since a fixed one turns a receiver that is down into a poll loop
//! hammering the store as fast as it can.
//!
//! The capture is a `tracing::Subscriber` written out here rather than a
//! dependency: this crate already depends on `tracing`, and the host runs on
//! runtime-owned threads, so a thread-scoped subscriber would not see it. What
//! is fixed is that a record carrying the five fields below is emitted at a
//! level an operator watches, together with the values of those fields.

#[path = "common/delivery.rs"]
mod delivery;
use delivery::{
    append_decided, scope_of, spawn_host, wait_until_loads, AppendRefusingEventStore, DeliverySaga,
    Ledger, RecordingEventStore, RecordingOffsetStore, TEST_DELIVERY_ID, TEST_POLL_INTERVAL,
};

use std::collections::BTreeMap;
use std::fmt::Debug;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use nitinol_eventsource::system::EventSourceSystem;
use nitinol_persistence::provider::{CustomStore, PersistenceProvider};
use nitinol_persistence::store::InMemoryEventStore;
use nitinol_persistence::AggregateId;
use nitinol_runtime::ProcessSystem;
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
    fn stall_records(&self, target: &str) -> Vec<Captured> {
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

    async fn wait_for_stall_record(&self, target: &str) -> Captured {
        let deadline = Instant::now() + delivery::PROBE_TIMEOUT;
        loop {
            if let Some(first) = self.stall_records(target).into_iter().next() {
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
async fn a_held_delivery_is_retried_with_a_growing_pause_and_is_diagnosable() {
    let capture = Capture::installed();

    // Given: a receiver that accepts commands and cannot record them.
    //
    // A receiving aggregate's stream is in the journal the system's provider
    // supplies, and there is no per-spawn store to override it with, so the
    // provider is assembled over the refusing store itself.
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps)
        .with_persistence(PersistenceProvider::new(
            CustomStore::builder()
                .eventstore(AppendRefusingEventStore)
                .build(),
        ))
        .build();
    let target = AggregateId::new("ledger-transient");
    let _unwritable = system.spawn_aggregate::<Ledger>(target.clone()).await;

    let recording = RecordingEventStore::new(InMemoryEventStore::default());
    let journal = recording.clone();
    let offsets = RecordingOffsetStore::new();
    let saga_id = SagaId::new("instance-1");
    let held = append_decided(&journal, &saga_id, 1, target.as_str(), "op-1").await;

    // When
    let _host = spawn_host::<DeliverySaga, _>(&system, &journal, &offsets, 1).await;
    let queries = wait_until_loads(&recording, 6, "the host must keep retrying the record").await;

    // Then: the record is asked for again, and again.
    for (nth, query) in queries.iter().enumerate() {
        assert_eq!(
            query.from_global_sequence,
            Some(held),
            "load call {nth} must still ask for the held record"
        );
    }
    assert!(
        offsets.stored(&scope_of(0)).is_none_or(|at| at < held),
        "a delivery that was never recorded at the receiver cannot be progress"
    );

    // And: the pause between attempts grows.
    let gaps = recording.load_gaps();
    let first = *gaps
        .first()
        .expect("six load calls leave five gaps between them");
    let last = *gaps
        .last()
        .expect("six load calls leave five gaps between them");
    assert!(
        last >= first * 2,
        "the pause must grow while the failure persists, so a receiver that is \
         down is not polled at full rate; gaps were {gaps:?}"
    );
    assert!(
        first >= TEST_POLL_INTERVAL,
        "even the first retry waits at least one poll interval; gaps were {gaps:?}"
    );

    // And: the hold is diagnosable.
    let record = capture.wait_for_stall_record(target.as_str()).await;
    let held_text = held.to_string();
    assert_eq!(
        record.fields.get(GLOBAL_SEQUENCE).map(String::as_str),
        Some(held_text.as_str()),
        "the record names the position the host is held at"
    );
    assert_eq!(
        record.fields.get(PARTITION).map(String::as_str),
        Some("0"),
        "the record names which partition is held, so a hold is attributable \
         in a host of many"
    );
    assert_eq!(
        record.fields.get(DELIVERY_ID).map(String::as_str),
        Some(TEST_DELIVERY_ID),
        "the record names which host is held"
    );
    assert_eq!(
        record.fields.get(RETRYABILITY).map(String::as_str),
        Some("Transient"),
        "the record names the classification the host reached, which is what \
         says whether an operator is waiting for a recovery or looking at a \
         verdict"
    );
    assert!(
        matches!(record.level, Level::WARN | Level::ERROR),
        "a hold is emitted at a level an operator watches, not one they have to \
         turn on; it was {:?}",
        record.level
    );

    // The window the gaps were measured over has to be long enough for the
    // growth above to be about backoff rather than about scheduling noise.
    assert!(
        gaps.iter().copied().sum::<Duration>() > TEST_POLL_INTERVAL * 6,
        "gaps were {gaps:?}"
    );
}
