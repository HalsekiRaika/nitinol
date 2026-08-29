//! Two interpreters of the conformance kit's `Ledger` decider.
//!
//! `ActivationInterpretation` is the one this crate ships: an aggregate
//! activation reached through `AggregateProxy`.  `DirectInterpretation` is an
//! external executor that reads the same `Decision` values itself — it replays
//! the stream, decides, appends once and applies — without the process runtime.
//! Both consume the very same `impl Decider<..> for Ledger`, which is what makes
//! their answers comparable.
//!
//! Each raw outcome is classified into the kit's vocabulary in exactly one
//! place per interpreter, so every clause of the suite judges the same verdict
//! instead of re-reading a raw error its own way.
//!
//! Each test binary compiles this module in full but uses only the subset it
//! needs, so per-binary dead code is expected.

#![allow(dead_code)]

use std::fmt::Debug;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use futures_core::future::BoxFuture;
use futures_util::TryStreamExt;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Level, Metadata, Subscriber};

use nitinol_conformance::{
    Balance, Fault, Interpretation, Interpreted, Interpreter, Ledger, LedgerEvent, LedgerNotOpen,
    LedgerRejection, MalformedLedgerEvent, Unanswered,
};
use nitinol_eventsource::codec::Codec;
use nitinol_eventsource::{
    Aggregate, AggregateProps, AggregateProxy, AskError, Decider, Decision, Event, ExecError, Query,
};
use nitinol_persistence::error::AppendError;
use nitinol_persistence::store::EventStore;
use nitinol_persistence::{AggregateId, AppendingEvent, LoadQuery};
use nitinol_runtime::ProcessSystem;

// The kit's wire format, wired to this crate's codec

/// Delegates to the kit's own encoding so both interpreters write bytes the
/// kit can read back when it inspects a stream.
pub struct LedgerCodec;

impl Codec<LedgerEvent> for LedgerCodec {
    type Error = MalformedLedgerEvent;

    fn encode(event: &LedgerEvent) -> Result<Bytes, Self::Error> {
        Ok(event.encode())
    }

    fn decode(payload: &[u8]) -> Result<LedgerEvent, Self::Error> {
        LedgerEvent::decode(payload)
    }
}

// Capturing the records a told refusal is surfaced through

/// One captured record, reduced to what a surfaced refusal is judged by: how
/// loud it was and what it named.
#[derive(Clone, Debug)]
struct Reported {
    level: Level,
    fields: String,
}

/// Renders every field of a record, whatever its type, into one string.
///
/// Only `record_debug` is implemented because every other `Visit` method falls
/// back to it.
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

/// The process-wide record capture an activation interpreter reads its
/// surfaced refusals from.
///
/// The activation surfaces a refusal nobody is waiting for through this crate's
/// tracing records, so that is where the capture has to sit; the kit only ever
/// sees the rendered result.
#[derive(Clone)]
pub struct ReportCapture(Arc<Mutex<Vec<Reported>>>);

impl ReportCapture {
    /// Install the capture as the process-wide subscriber.
    ///
    /// A global subscriber can be installed once per process, so a test binary
    /// that calls this holds exactly one test.
    pub fn install() -> Self {
        let reported = Arc::new(Mutex::new(Vec::new()));
        tracing::subscriber::set_global_default(Recorder {
            reported: Arc::clone(&reported),
        })
        .expect("a binary that installs the capture holds one test, so nothing else installs one");
        Self(reported)
    }

    /// Rendered records at warning level or louder that name `ledger`.
    ///
    /// Naming the aggregate is what makes a record usable as a report of *this*
    /// interpreter's refusal rather than of some other activation's.
    fn naming(&self, ledger: &str) -> Vec<String> {
        self.0
            .lock()
            .expect(RECORD_LOCK)
            .iter()
            .filter(|record| record.level <= Level::WARN && record.fields.contains(ledger))
            .map(|record| record.fields.clone())
            .collect()
    }
}

// The interpreter this crate ships: an aggregate activation

pub struct ActivationInterpretation {
    process_system: ProcessSystem,
    reports: ReportCapture,
}

impl ActivationInterpretation {
    pub fn new(process_system: ProcessSystem, reports: ReportCapture) -> Self {
        Self {
            process_system,
            reports,
        }
    }
}

impl Interpretation for ActivationInterpretation {
    type Interpreter = ActivationInterpreter;

    fn interpret(
        &self,
        ledger: AggregateId,
        store: Arc<dyn EventStore>,
    ) -> BoxFuture<'_, Self::Interpreter> {
        Box::pin(async move {
            let proxy = AggregateProps::<Ledger>::new(ledger.clone(), store)
                .with_codec(Arc::new(LedgerCodec))
                .spawn(&self.process_system)
                .await;
            ActivationInterpreter {
                proxy,
                ledger,
                reports: self.reports.clone(),
            }
        })
    }
}

pub struct ActivationInterpreter {
    proxy: AggregateProxy<Ledger>,
    ledger: AggregateId,
    reports: ReportCapture,
}

impl Interpreter for ActivationInterpreter {
    fn ask<C>(&self, cmd: C) -> BoxFuture<'_, Interpreted<<Ledger as Decider<C>>::Output>>
    where
        Ledger: Decider<C, Rejection = LedgerRejection>,
        C: Send + Sync + 'static,
        <Ledger as Decider<C>>::Output: Send + 'static,
    {
        Box::pin(async move {
            match self.proxy.ask(cmd).await {
                Ok(output) => Interpreted::Answered(output),
                Err(AskError::Rejection(rejection)) => Interpreted::Refused(rejection),
                Err(AskError::AlreadyCreated) => Interpreted::AlreadyCreated,
                Err(AskError::Persist(failure)) => Interpreted::Failed(Fault::new(failure)),
                Err(AskError::Send(failure)) => Interpreted::Failed(Fault::new(failure)),
            }
        })
    }

    fn tell<C>(&self, cmd: C) -> BoxFuture<'_, Result<(), Fault>>
    where
        Ledger: Decider<C, Rejection = LedgerRejection>,
        C: Send + Sync + 'static,
    {
        Box::pin(async move { self.proxy.tell(cmd).await.map_err(Fault::new) })
    }

    fn exec<M>(&self, msg: M) -> BoxFuture<'_, Result<<Ledger as Query<M>>::Response, Unanswered>>
    where
        Ledger: Query<M, Error = LedgerNotOpen>,
        M: Send + Sync + 'static,
        <Ledger as Query<M>>::Response: Send + 'static,
    {
        Box::pin(async move {
            match self.proxy.exec(msg).await {
                Ok(response) => Ok(response),
                Err(ExecError::Domain(unanswerable)) => Err(Unanswered::Domain(unanswerable)),
                Err(ExecError::Send(failure)) => Err(Unanswered::Failed(Fault::new(failure))),
            }
        })
    }

    fn quiesce(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            // The activation's queue is FIFO, so an answered question proves
            // every command queued before it has already been handled. Waiting
            // on a clock would prove nothing.
            let _ = self.proxy.exec(Balance).await;
        })
    }

    fn surfaced_refusals(&self) -> Vec<String> {
        self.reports.naming(self.ledger.as_str())
    }
}

// An external executor: the same decisions, carried out without the runtime

pub struct DirectInterpretation;

impl Interpretation for DirectInterpretation {
    type Interpreter = DirectInterpreter;

    fn interpret(
        &self,
        ledger: AggregateId,
        store: Arc<dyn EventStore>,
    ) -> BoxFuture<'_, Self::Interpreter> {
        Box::pin(async move {
            let replayed = replay(&store, &ledger).await;
            DirectInterpreter {
                ledger,
                store,
                writer: tokio::sync::Mutex::new(replayed),
                refusals: Mutex::new(Vec::new()),
            }
        })
    }
}

/// The state this executor decides from, and the sequence it numbers its next
/// append from.
struct Replayed {
    state: Ledger,
    sequence: u64,
}

/// What became of the facts of an acceptance — without the answer, which only
/// the ask path still owes its caller.
enum Recorded {
    Committed,
    AlreadyCreated,
    Failed(Fault),
}

async fn replay(store: &Arc<dyn EventStore>, ledger: &AggregateId) -> Replayed {
    let loaded: Vec<_> = store
        .load(LoadQuery::by_stream(ledger))
        .await
        .expect("the conformance store must answer a load")
        .try_collect()
        .await
        .expect("collecting the replayed stream must succeed");

    let mut replayed = Replayed {
        state: Ledger::default(),
        sequence: 0,
    };
    for event in loaded {
        let fact = LedgerEvent::decode(&event.payload)
            .expect("a stream written in the kit's own format must decode");
        replayed.state.apply(fact);
        replayed.sequence = event.sequence;
    }
    replayed
}

/// Why the refusal buffer's lock cannot be poisoned: it guards a push and a
/// read of a `Vec`, and no holder can panic.
const REFUSAL_LOCK: &str = "the refusal buffer guards only a push and a read, so no holder panics";

pub struct DirectInterpreter {
    ledger: AggregateId,
    store: Arc<dyn EventStore>,
    writer: tokio::sync::Mutex<Replayed>,
    refusals: Mutex<Vec<String>>,
}

impl DirectInterpreter {
    /// Write the facts of one acceptance as a single append, and advance the
    /// state only once the store has taken them.
    async fn record(&self, writer: &mut Replayed, events: Vec<LedgerEvent>) -> Recorded {
        if events.is_empty() {
            return Recorded::Committed;
        }

        let mut sequence = writer.sequence;
        let mut appending = Vec::with_capacity(events.len());
        for event in &events {
            sequence += 1;
            appending.push(AppendingEvent {
                sequence,
                event_type: event.variant(),
                payload: event.encode(),
                occurred_at: jiff::Timestamp::now(),
            });
        }

        match self.store.append(self.ledger.as_str(), appending).await {
            Ok(_) => {}
            // Appending from sequence zero is a creation, so a conflict there
            // says the aggregate already exists rather than that this executor
            // was overtaken.
            Err(AppendError::SequenceConflict(_)) if writer.sequence == 0 => {
                return Recorded::AlreadyCreated
            }
            Err(failure) => return Recorded::Failed(Fault::new(failure)),
        }

        writer.sequence = sequence;
        for event in events {
            writer.state.apply(event);
        }
        Recorded::Committed
    }
}

impl Interpreter for DirectInterpreter {
    fn ask<C>(&self, cmd: C) -> BoxFuture<'_, Interpreted<<Ledger as Decider<C>>::Output>>
    where
        Ledger: Decider<C, Rejection = LedgerRejection>,
        C: Send + Sync + 'static,
        <Ledger as Decider<C>>::Output: Send + 'static,
    {
        Box::pin(async move {
            let mut writer = self.writer.lock().await;
            match writer.state.decide(cmd) {
                Decision::Reject(rejection) => Interpreted::Refused(rejection),
                Decision::Accept { events, output } => {
                    match self.record(&mut writer, events).await {
                        Recorded::Committed => Interpreted::Answered(output),
                        Recorded::AlreadyCreated => Interpreted::AlreadyCreated,
                        Recorded::Failed(fault) => Interpreted::Failed(fault),
                    }
                }
            }
        })
    }

    fn tell<C>(&self, cmd: C) -> BoxFuture<'_, Result<(), Fault>>
    where
        Ledger: Decider<C, Rejection = LedgerRejection>,
        C: Send + Sync + 'static,
    {
        Box::pin(async move {
            let mut writer = self.writer.lock().await;
            // The answer is dropped here, before the append is awaited: nobody
            // is waiting for it, and holding it would demand `Send` of a value
            // no one reads. A refusal is not dropped — it is surfaced instead.
            let events = match writer.state.decide(cmd) {
                Decision::Accept { events, .. } => events,
                Decision::Reject(rejection) => {
                    self.refusals
                        .lock()
                        .expect(REFUSAL_LOCK)
                        .push(rejection.to_string());
                    return Ok(());
                }
            };

            match self.record(&mut writer, events).await {
                Recorded::Committed | Recorded::AlreadyCreated => Ok(()),
                Recorded::Failed(fault) => Err(fault),
            }
        })
    }

    fn exec<M>(&self, msg: M) -> BoxFuture<'_, Result<<Ledger as Query<M>>::Response, Unanswered>>
    where
        Ledger: Query<M, Error = LedgerNotOpen>,
        M: Send + Sync + 'static,
        <Ledger as Query<M>>::Response: Send + 'static,
    {
        Box::pin(async move {
            let writer = self.writer.lock().await;
            writer.state.query(msg).map_err(Unanswered::Domain)
        })
    }

    fn quiesce(&self) -> BoxFuture<'_, ()> {
        // Every dispatch has already run to completion by the time it returns,
        // so there is nothing left to wait for.
        Box::pin(async {})
    }

    fn surfaced_refusals(&self) -> Vec<String> {
        self.refusals.lock().expect(REFUSAL_LOCK).clone()
    }
}
