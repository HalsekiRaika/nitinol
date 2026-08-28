// A command refused on the tell path is reported, not lost (L-5).
//
// The tell path has no caller waiting for an answer, so a refusal has nowhere
// to be returned.  Dropping it there would make a command that was refused
// indistinguishable from one that was carried out, for everyone: the sender
// already moved on, and the stream — correctly — holds nothing either way.  So
// the refusal is written to the one channel that does not need a receiver: the
// crate's own tracing records.
//
// The subscriber that captures those records is installed globally, which can
// happen once per process, so this file holds exactly one test.

use std::fmt::Debug;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use serde::{Deserialize, Serialize};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Level, Metadata, Subscriber};

use nitinol_eventsource::{
    codec::Codec, Aggregate, AggregateProps, Decider, Decision, Event, Query,
};
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::{AggregateId, EventType, Family, TypeName};
use nitinol_runtime::ProcessSystem;

// Fixtures: event

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Incremented;

impl Event for Incremented {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("tell-refusal"), TypeName::new("Incremented"));
}

// Fixtures: aggregate

#[derive(Default)]
struct Counter {
    value: u64,
}

impl Aggregate for Counter {
    type Event = Incremented;

    fn apply(&mut self, _event: Incremented) {
        self.value += 1;
    }
}

// Fixtures: commands and query

struct Increment;
struct IncrementIfLessThan(u64);
struct GetCount;

#[derive(Debug, thiserror::Error)]
#[error("counter already at {0}")]
struct AtMaxError(u64);

impl Decider<Increment> for Counter {
    type Output = u64;
    type Rejection = std::convert::Infallible;

    fn decide(&self, _cmd: Increment) -> Decision<Incremented, u64, Self::Rejection> {
        Decision::persist(vec![Incremented]).output(self.value + 1)
    }
}

impl Decider<IncrementIfLessThan> for Counter {
    type Output = u64;
    type Rejection = AtMaxError;

    fn decide(&self, cmd: IncrementIfLessThan) -> Decision<Incremented, u64, AtMaxError> {
        if self.value >= cmd.0 {
            return Decision::reject(AtMaxError(self.value));
        }
        Decision::persist(vec![Incremented]).output(self.value + 1)
    }
}

impl Query<GetCount> for Counter {
    type Response = u64;
    type Error = std::convert::Infallible;

    fn query(&self, _msg: GetCount) -> Result<u64, Self::Error> {
        Ok(self.value)
    }
}

// Fixtures: codec

#[derive(Default)]
struct JsonCodec;

impl<E: Serialize + for<'de> Deserialize<'de>> Codec<E> for JsonCodec {
    type Error = serde_json::Error;

    fn encode(event: &E) -> Result<Bytes, Self::Error> {
        serde_json::to_vec(event).map(Bytes::from)
    }

    fn decode(payload: &[u8]) -> Result<E, Self::Error> {
        serde_json::from_slice(payload)
    }
}

// Fixtures: capturing the crate's tracing records

/// One captured record, reduced to what this test judges: how loud it was and
/// what it named.
#[derive(Clone, Debug)]
struct Reported {
    level: Level,
    fields: String,
}

/// Renders every field of a record, whatever its type, into one string.
///
/// Only `record_debug` is implemented because every other `Visit` method falls
/// back to it, so a value recorded as a string, a number or a display value all
/// arrive here.
#[derive(Default)]
struct FieldText(String);

impl Visit for FieldText {
    fn record_debug(&mut self, field: &Field, value: &dyn Debug) {
        self.0.push_str(&format!("{}={:?} ", field.name(), value));
    }
}

struct Recorder {
    reported: Arc<Mutex<Vec<Reported>>>,
}

/// Why the capture buffer's lock cannot be poisoned: it guards a push and a
/// read of a `Vec`, and no holder can panic.
const RECORD_LOCK: &str = "the capture buffer guards only a push and a read, so no holder panics";

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
        let mut fields = FieldText::default();
        event.record(&mut fields);
        self.reported.lock().expect(RECORD_LOCK).push(Reported {
            level: *event.metadata().level(),
            fields: fields.0,
        });
    }

    fn enter(&self, _span: &Id) {}

    fn exit(&self, _span: &Id) {}
}

/// Captured records at warning level or louder that name `aggregate_id`.
///
/// Naming the aggregate is what makes the record usable: a warning that does
/// not say which aggregate refused what is not a report of this refusal.
fn refusals_reported_for(reported: &Mutex<Vec<Reported>>, aggregate_id: &str) -> Vec<Reported> {
    reported
        .lock()
        .expect(RECORD_LOCK)
        .iter()
        .filter(|record| record.level <= Level::WARN && record.fields.contains(aggregate_id))
        .cloned()
        .collect()
}

/// A command the tell path refuses must leave a report behind; one it carries
/// out must not.
///
/// Both halves run against the same aggregate so that the report is attributed
/// to the refusal rather than to telling anything at all.
#[tokio::test]
async fn a_refused_told_command_is_reported_and_a_carried_out_one_is_not() {
    // Given
    let reported = Arc::new(Mutex::new(Vec::new()));
    tracing::subscriber::set_global_default(Recorder {
        reported: Arc::clone(&reported),
    })
    .expect("this test binary holds one test, so nothing else installs a subscriber");

    let ps = ProcessSystem::new().await;
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let id = AggregateId::new("tell-refusal-reported");
    let proxy = AggregateProps::<Counter>::new(id.clone(), Arc::clone(&store))
        .with_codec(Arc::new(JsonCodec))
        .spawn(&ps)
        .await;

    // When: the queue is FIFO, so the query is answered only after the refused
    // command has been handled and whatever it reports has been reported.
    proxy
        .tell(IncrementIfLessThan(0))
        .await
        .expect("the command must be accepted for delivery");
    let after_refusal = proxy
        .exec(GetCount)
        .await
        .expect("a refused command must not stop the activation that refused it");

    // Then
    assert_eq!(after_refusal, 0, "a refused command must change nothing");
    let refusals = refusals_reported_for(&reported, id.as_str());
    assert_eq!(
        refusals.len(),
        1,
        "a command refused with nobody waiting must leave exactly one report naming the \
         aggregate that refused it, got {refusals:?}"
    );
    // Taken from the rejection value rather than written out, so the assertion
    // follows the decider's own wording instead of a copy of it.
    let refusal = AtMaxError(0).to_string();
    assert!(
        refusals[0].fields.contains(&refusal),
        "the report must carry the refusal itself: a warning that some command was refused, \
         without saying which verdict the decider reached, leaves the refusal as unreadable \
         as dropping it would (L-5); expected {refusal:?} in {refusals:?}"
    );

    // When: the same aggregate is told a command it accepts
    reported.lock().expect(RECORD_LOCK).clear();
    proxy
        .tell(Increment)
        .await
        .expect("the command must be accepted for delivery");
    let after_acceptance = proxy
        .exec(GetCount)
        .await
        .expect("the activation must answer after carrying the command out");

    // Then
    assert_eq!(
        after_acceptance, 1,
        "the accepted command must have been carried out"
    );
    let false_reports = refusals_reported_for(&reported, id.as_str());
    assert!(
        false_reports.is_empty(),
        "a command that was carried out must not be reported as refused, got {false_reports:?}"
    );
}
