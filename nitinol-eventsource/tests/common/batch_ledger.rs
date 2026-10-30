//! One timeline that a projector's transaction provider, its offset store and
//! its read model all write to.
//!
//! Which calls a batch makes, and in what order, is only visible when every
//! party records into the same sequence: a count of commits cannot say whether
//! the offset was saved before or after the handlers ran, and a count of saves
//! cannot say which transaction a save belonged to.
//!
//! The transaction is a staging area: what a handler writes through
//! `ProjectionContext::tx` and the offset saved inside it reach the read model
//! and the offset slot only on `commit`, and `rollback` discards both. That is
//! what lets a clause say "no record of the batch reached the read model".

use std::collections::HashMap;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;

use nitinol_eventsource::codec::Codec;
use nitinol_eventsource::{Event, ProjectionContext, Projector, TxProvider};
use nitinol_persistence::error::OffsetError;
use nitinol_persistence::store::{EventStore, OffsetScope, OffsetStore};
use nitinol_persistence::{AggregateId, AppendingEvent, EventType, Family, TypeName};

const LEDGER_LOCK: &str = "the ledger lock guards only pushes and copies, so no holder panics";

/// How long a wait below gives the projector before it is a failure rather
/// than a slow machine.
pub const WAIT_TIMEOUT: Duration = Duration::from_secs(10);

const WAIT_STEP: Duration = Duration::from_millis(5);

// The followed event

#[derive(Clone)]
pub struct Evt;

pub struct UnitCodec;

impl Codec<Evt> for UnitCodec {
    type Error = std::convert::Infallible;

    fn encode(_event: &Evt) -> Result<Bytes, Self::Error> {
        Ok(Bytes::new())
    }

    fn decode(_payload: &[u8]) -> Result<Evt, Self::Error> {
        Ok(Evt)
    }
}

impl Event for Evt {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("projector.batch"), TypeName::new("Evt"));

    type Codec = UnitCodec;
}

/// A type no projector here registers, so a record of it has no handler.
pub const UNREGISTERED_EVENT_TYPE: EventType = EventType::new(
    Family::new("projector.batch"),
    TypeName::new("Unregistered"),
);

/// Append records of `event_type` numbered `from..=through` to `stream`.
pub async fn append_range(
    store: &impl EventStore,
    stream: &AggregateId,
    from: u64,
    through: u64,
    event_type: EventType,
) {
    let events = (from..=through)
        .map(|sequence| AppendingEvent {
            sequence,
            event_type,
            payload: Bytes::new(),
            occurred_at: jiff::Timestamp::now(),
        })
        .collect();
    store
        .append(stream.as_str(), events)
        .await
        .expect("appending the next sequences of a stream must succeed");
}

// The timeline

/// One call a projector made, as the party that received it saw it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    Begin {
        tx: u64,
    },
    /// A handler ran for `sequence`, inside the transaction `tx` if it was
    /// handed one, and `applied` says whether it returned `Ok`.
    Project {
        sequence: u64,
        tx: Option<u64>,
        applied: bool,
    },
    Save {
        sequence: u64,
        tx: Option<u64>,
    },
    Commit {
        tx: u64,
    },
    Rollback {
        tx: u64,
    },
}

#[derive(Default)]
struct LedgerState {
    steps: Vec<Step>,
    /// What reached the read model: committed writes, or writes made with no
    /// transaction at all.
    read_model: Vec<u64>,
    offsets: HashMap<String, u64>,
    transactions_begun: u64,
}

/// The shared record. Its `Clone` is the handle's.
#[derive(Clone, Default)]
pub struct Ledger {
    inner: Arc<Mutex<LedgerState>>,
}

impl Ledger {
    fn lock(&self) -> MutexGuard<'_, LedgerState> {
        self.inner.lock().expect(LEDGER_LOCK)
    }

    fn push(&self, step: Step) {
        self.lock().steps.push(step);
    }

    pub fn steps(&self) -> Vec<Step> {
        self.lock().steps.clone()
    }

    pub fn read_model(&self) -> Vec<u64> {
        self.lock().read_model.clone()
    }

    pub fn offset_of(&self, scope: &OffsetScope) -> Option<u64> {
        self.lock().offsets.get(scope.as_str()).copied()
    }

    pub fn saves(&self) -> Vec<Step> {
        self.steps_matching(|s| matches!(s, Step::Save { .. }))
    }

    pub fn commits(&self) -> usize {
        self.steps_matching(|s| matches!(s, Step::Commit { .. }))
            .len()
    }

    pub fn rollbacks(&self) -> usize {
        self.steps_matching(|s| matches!(s, Step::Rollback { .. }))
            .len()
    }

    fn steps_matching(&self, keep: impl Fn(&Step) -> bool) -> Vec<Step> {
        self.lock()
            .steps
            .iter()
            .filter(|s| keep(s))
            .cloned()
            .collect()
    }

    /// Wait until `condition` holds of this ledger, and answer whether it did
    /// before [`WAIT_TIMEOUT`], so the caller asserts on what it saw.
    pub async fn wait_for(&self, condition: impl Fn(&Ledger) -> bool) -> bool {
        let deadline = Instant::now() + WAIT_TIMEOUT;
        loop {
            if condition(self) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(WAIT_STEP).await;
        }
    }
}

// Failures

/// Fail the handler for `sequence` the first `times` it is called for it.
#[derive(Clone)]
pub struct Failure {
    sequence: Option<u64>,
    remaining: Arc<AtomicUsize>,
}

/// A budget that is never spent.
pub const PERMANENT: usize = usize::MAX;

impl Failure {
    pub fn none() -> Self {
        Self {
            sequence: None,
            remaining: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn at(sequence: u64, times: usize) -> Self {
        Self {
            sequence: Some(sequence),
            remaining: Arc::new(AtomicUsize::new(times)),
        }
    }

    fn strikes(&self, sequence: u64) -> bool {
        self.sequence == Some(sequence)
            && self
                .remaining
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| match n {
                    0 => None,
                    PERMANENT => Some(PERMANENT),
                    n => Some(n - 1),
                })
                .is_ok()
    }
}

#[derive(Debug, thiserror::Error)]
#[error("intentional project failure")]
pub struct ProjectFailed;

// The transaction and its provider

pub struct MockTx {
    id: u64,
    writes: Vec<u64>,
    offset: Option<(String, u64)>,
}

pub struct LedgerTxProvider {
    ledger: Ledger,
}

impl LedgerTxProvider {
    pub fn new(ledger: &Ledger) -> Self {
        Self {
            ledger: ledger.clone(),
        }
    }
}

#[async_trait]
impl TxProvider for LedgerTxProvider {
    type Tx = MockTx;
    type Error = std::convert::Infallible;

    async fn begin(&self) -> Result<MockTx, Self::Error> {
        let mut state = self.ledger.lock();
        state.transactions_begun += 1;
        let id = state.transactions_begun;
        state.steps.push(Step::Begin { tx: id });
        Ok(MockTx {
            id,
            writes: Vec::new(),
            offset: None,
        })
    }

    async fn commit(&self, tx: MockTx) -> Result<(), Self::Error> {
        let mut state = self.ledger.lock();
        state.steps.push(Step::Commit { tx: tx.id });
        state.read_model.extend(tx.writes);
        if let Some((scope, sequence)) = tx.offset {
            state.offsets.insert(scope, sequence);
        }
        Ok(())
    }

    async fn rollback(&self, tx: MockTx) {
        self.ledger.push(Step::Rollback { tx: tx.id });
    }
}

// The offset store

/// An offset store over the ledger. With `Tx = MockTx` a save handed the
/// transaction is staged in it; a save handed none lands at once.
pub struct LedgerOffsetStore<Tx> {
    ledger: Ledger,
    _tx: PhantomData<fn() -> Tx>,
}

impl<Tx> Clone for LedgerOffsetStore<Tx> {
    fn clone(&self) -> Self {
        Self {
            ledger: self.ledger.clone(),
            _tx: PhantomData,
        }
    }
}

impl<Tx> LedgerOffsetStore<Tx> {
    pub fn new(ledger: &Ledger) -> Self {
        Self {
            ledger: ledger.clone(),
            _tx: PhantomData,
        }
    }

    fn load_from_ledger(&self, scope: &OffsetScope) -> Option<u64> {
        self.ledger.offset_of(scope)
    }

    fn save_at_once(&self, scope: &OffsetScope, sequence: u64) {
        let mut state = self.ledger.lock();
        state.steps.push(Step::Save { sequence, tx: None });
        state.offsets.insert(scope.as_str().to_owned(), sequence);
    }
}

#[async_trait]
impl OffsetStore for LedgerOffsetStore<()> {
    type Tx = ();

    async fn load(&self, scope: &OffsetScope) -> Result<Option<u64>, OffsetError> {
        Ok(self.load_from_ledger(scope))
    }

    async fn save(
        &self,
        scope: &OffsetScope,
        sequence: u64,
        _tx: Option<&mut ()>,
    ) -> Result<(), OffsetError> {
        self.save_at_once(scope, sequence);
        Ok(())
    }
}

#[async_trait]
impl OffsetStore for LedgerOffsetStore<MockTx> {
    type Tx = MockTx;

    async fn load(&self, scope: &OffsetScope) -> Result<Option<u64>, OffsetError> {
        Ok(self.load_from_ledger(scope))
    }

    async fn save(
        &self,
        scope: &OffsetScope,
        sequence: u64,
        tx: Option<&mut MockTx>,
    ) -> Result<(), OffsetError> {
        match tx {
            Some(tx) => {
                self.ledger.push(Step::Save {
                    sequence,
                    tx: Some(tx.id),
                });
                tx.offset = Some((scope.as_str().to_owned(), sequence));
            }
            None => self.save_at_once(scope, sequence),
        }
        Ok(())
    }
}

// The read models

/// A read model written through the transaction it is handed.
pub struct TxReadModel {
    pub ledger: Ledger,
    pub failure: Failure,
}

#[async_trait]
impl Projector<Evt, MockTx> for TxReadModel {
    type Error = ProjectFailed;

    async fn project(
        &mut self,
        _event: Evt,
        ctx: &mut ProjectionContext<'_, MockTx>,
    ) -> Result<(), Self::Error> {
        let sequence = ctx.current_sequence();
        let failing = self.failure.strikes(sequence);
        let tx = ctx.tx().map(|tx| {
            if !failing {
                tx.writes.push(sequence);
            }
            tx.id
        });
        self.ledger.push(Step::Project {
            sequence,
            tx,
            applied: !failing,
        });
        if failing {
            Err(ProjectFailed)
        } else {
            Ok(())
        }
    }
}

/// A read model with no transaction: what it applies lands at once.
pub struct PlainReadModel {
    pub ledger: Ledger,
    pub failure: Failure,
}

#[async_trait]
impl Projector<Evt> for PlainReadModel {
    type Error = ProjectFailed;

    async fn project(
        &mut self,
        _event: Evt,
        ctx: &mut ProjectionContext<'_, ()>,
    ) -> Result<(), Self::Error> {
        let sequence = ctx.current_sequence();
        let failing = self.failure.strikes(sequence);
        let mut state = self.ledger.lock();
        if !failing {
            state.read_model.push(sequence);
        }
        state.steps.push(Step::Project {
            sequence,
            tx: None,
            applied: !failing,
        });
        drop(state);
        if failing {
            Err(ProjectFailed)
        } else {
            Ok(())
        }
    }
}
