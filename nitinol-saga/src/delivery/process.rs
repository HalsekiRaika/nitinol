//! One partition of a delivery host: the loop that reads, delivers, and
//! records.

use std::future::Future;
use std::marker::PhantomData;
use std::num::NonZeroUsize;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{FutureExt, StreamExt};

use nitinol_eventsource::codec;
use nitinol_eventsource::system::SystemHandle;
use nitinol_eventsource::Event;
use nitinol_persistence::store::{EventStore, OffsetScope, OffsetStore};
use nitinol_persistence::{LoadQuery, LoadedEvent};
use nitinol_runtime::error::HandlerError;
use nitinol_runtime::process::{Driver, Process, ProcessContext, Receive};

use crate::delivery::error::SagaDeliveryError;
use crate::delivery::id::SagaDeliveryId;
use crate::delivery::outcome::{Hold, HoldCause, RecordOutcome, TickOutcome};
use crate::delivery::partition::partition_of;
use crate::saga::Saga;

/// How many times the pause between attempts may double.
///
/// A hold that persists must stop growing somewhere, or a receiver that comes
/// back after a long outage waits hours to be noticed. Doubling this many
/// times takes a poll interval into the minutes, which is slow enough not to
/// hammer whatever is broken and soon enough to pick up a recovery.
const MAX_BACKOFF_DOUBLINGS: u32 = 10;

/// Where a partition has got to, once it has read its recorded progress.
///
/// The two are kept together because the invariant between them is what the
/// design rests on, and because neither is meaningful before the recorded
/// progress has been read — which is what the absence of this value says.
#[derive(Clone, Copy)]
struct Position {
    /// The last record this partition finished with.
    ///
    /// Never past a record whose work is still owed.
    cursor: u64,
    /// The last position the store has taken.
    ///
    /// At or behind `cursor` in ordinary running: the host may be further along
    /// than the store knows, which costs a redelivery after a crash and never a
    /// delivery nothing points at. The two move together only when an operator
    /// moves them.
    durable: u64,
}

/// Ask a running partition to treat everything up to `to` as finished with.
///
/// Addressed to the partition rather than written straight to the store: a
/// write the running partition did not see would leave it reading from where it
/// was, so the operation would appear to work and change nothing until the next
/// restart.
pub(crate) struct AdvanceOffset {
    pub(crate) to: u64,
}

/// One partition of a delivery host.
pub struct SagaDeliveryPartition<S: Saga, Cs: OffsetStore> {
    delivery_id: SagaDeliveryId,
    partition: usize,
    partitions: NonZeroUsize,
    scope: OffsetScope,
    journal: Arc<dyn EventStore>,
    offsets: Arc<Cs>,
    system: SystemHandle,
    poll_interval: Duration,
    /// Absent until the recorded progress has been read.
    ///
    /// A partition rebuilt by supervision starts here again, so a restart
    /// resumes from what the store holds then rather than from a position
    /// captured when the host was wired.
    position: Option<Position>,
    /// How many turns in a row have ended held, which is what the pause grows
    /// with.
    consecutive_holds: u32,
    /// The earliest time the partition may try again.
    resume_at: Option<tokio::time::Instant>,
    _saga: PhantomData<fn() -> S>,
}

impl<S, Cs> SagaDeliveryPartition<S, Cs>
where
    S: Saga,
    Cs: OffsetStore + 'static,
{
    pub(crate) fn new(
        delivery_id: SagaDeliveryId,
        partition: usize,
        partitions: NonZeroUsize,
        journal: Arc<dyn EventStore>,
        offsets: Arc<Cs>,
        system: SystemHandle,
        poll_interval: Duration,
    ) -> Self {
        Self {
            scope: delivery_id.offset_scope(partition),
            delivery_id,
            partition,
            partitions,
            journal,
            offsets,
            system,
            poll_interval,
            position: None,
            consecutive_holds: 0,
            resume_at: None,
            _saga: PhantomData,
        }
    }

    /// One turn of the loop.
    async fn tick(&mut self) {
        if let Some(resume_at) = self.resume_at {
            if tokio::time::Instant::now() < resume_at {
                // Not asking is the whole of the pause: a held partition that
                // kept loading would poll a broken receiver at full rate.
                return;
            }
        }

        match self.poll_once().await {
            TickOutcome::Clear => {
                self.consecutive_holds = 0;
                self.resume_at = None;
            }
            TickOutcome::Held(hold) => {
                hold.report(&self.delivery_id, self.partition);
                self.consecutive_holds = self.consecutive_holds.saturating_add(1);
                self.resume_at = Some(tokio::time::Instant::now() + self.backoff());
            }
        }
    }

    /// How long to wait before the next attempt.
    fn backoff(&self) -> Duration {
        let doublings = self.consecutive_holds.min(MAX_BACKOFF_DOUBLINGS);
        self.poll_interval.saturating_mul(1u32 << doublings)
    }

    /// Read the records above where this partition got to, deliver what they
    /// imply, and record how far it reached.
    ///
    /// The turn's hold is the first step that did not finish. Recording still
    /// runs after a record held, so progress made before the held record is
    /// not lost, but it does not replace what the partition is waiting on.
    async fn poll_once(&mut self) -> TickOutcome {
        let mut position = match self.resolved_position().await {
            Ok(position) => position,
            Err(hold) => return TickOutcome::Held(hold),
        };

        let query = LoadQuery {
            event_type_prefix: Some(<S::Event as Event>::EVENT_TYPE.to_path()),
            from_global_sequence: Some(position.cursor + 1),
            ..Default::default()
        };

        let mut held = None;
        match self.journal.load(query).await {
            Ok(stream) => {
                let mut stream = stream;
                while let Some(item) = stream.next().await {
                    let outcome = match item {
                        Ok(loaded) => self.judge(loaded).await,
                        Err(failure) => RecordOutcome::Held(Hold::transient(
                            HoldCause::RecordsUnread {
                                position: position.cursor + 1,
                            },
                            failure,
                        )),
                    };
                    match outcome {
                        RecordOutcome::Finished { position: reached } => position.cursor = reached,
                        RecordOutcome::Held(hold) => {
                            held = Some(hold);
                            break;
                        }
                    }
                }
            }
            Err(failure) => {
                held = Some(Hold::transient(
                    HoldCause::RecordsUnread {
                        position: position.cursor + 1,
                    },
                    failure,
                ));
            }
        }

        let unrecorded = self.record(&mut position).await;
        self.position = Some(position);

        match held.or(unrecorded) {
            Some(hold) => TickOutcome::Held(hold),
            None => TickOutcome::Clear,
        }
    }

    /// Where this partition resumes from, reading the store the first time and
    /// after every restart.
    async fn resolved_position(&mut self) -> Result<Position, Hold> {
        if let Some(position) = self.position {
            return Ok(position);
        }
        match self.offsets.load(&self.scope).await {
            Ok(stored) => {
                let at = stored.unwrap_or(0);
                let position = Position {
                    cursor: at,
                    durable: at,
                };
                self.position = Some(position);
                Ok(position)
            }
            Err(failure) => Err(Hold::transient(HoldCause::ProgressUnread, failure)),
        }
    }

    /// Offer the store the position this partition has reached, if it is ahead
    /// of what the store holds.
    ///
    /// Only ever a strict move forward, so a backend that takes none but
    /// forward moves and one that overwrites whatever it is handed answer a
    /// correct host identically.
    async fn record(&self, position: &mut Position) -> Option<Hold> {
        if position.cursor <= position.durable {
            return None;
        }
        match self.offsets.save(&self.scope, position.cursor, None).await {
            Ok(()) => {
                position.durable = position.cursor;
                None
            }
            Err(failure) => {
                // The cursor is not wound back. The deliveries up to it were
                // carried out, and offering a position behind them again would
                // redeliver work for no gain; what is owed is the records above
                // the cursor, and they are still above it.
                Some(Hold::transient(
                    HoldCause::ProgressUnrecorded {
                        position: position.cursor,
                    },
                    failure,
                ))
            }
        }
    }

    /// What this partition owes on one record of its subscription.
    async fn judge(&self, loaded: LoadedEvent) -> RecordOutcome {
        let position = loaded.global_sequence;

        // A record of another partition's stream is nothing this one owes, so
        // standing at it is right: holding the cursor below records it will
        // never deliver would have the store re-scan the same tail forever.
        if partition_of(&loaded.stream_key, self.partitions) != self.partition {
            return RecordOutcome::Finished { position };
        }

        let event = match codec::decode::<<S::Event as Event>::Codec, S::Event>(&loaded.payload) {
            Ok(event) => event,
            Err(failure) => {
                return RecordOutcome::Held(Hold::permanent(
                    HoldCause::RecordUnreadable { position },
                    failure,
                ))
            }
        };

        // Delivered one at a time, in the order the mapping declared them, so
        // that one instance's decisions reach their targets in the order the
        // saga decided them. The first failure ends the record: the rest of its
        // deliveries are re-derived and re-issued when it is asked for again.
        for delivery in S::dispatch(&event) {
            let target = delivery.target().clone();
            if let Err(failure) = delivery.deliver(self.system.clone()).await {
                return RecordOutcome::Held(Hold::delivery(position, target, failure));
            }
        }

        RecordOutcome::Finished { position }
    }
}

impl<S, Cs> Process for SagaDeliveryPartition<S, Cs>
where
    S: Saga,
    Cs: OffsetStore + 'static,
{
}

impl<S, Cs> Receive<AdvanceOffset> for SagaDeliveryPartition<S, Cs>
where
    S: Saga,
    Cs: OffsetStore + 'static,
{
    type Response = ();
    type Error = SagaDeliveryError;

    /// Move this partition on, past whatever it is held at.
    ///
    /// The store is written first and the running partition follows it, so a
    /// move the store refused changes nothing here either.
    async fn recv(
        &mut self,
        msg: AdvanceOffset,
        _ctx: &mut ProcessContext<Self>,
    ) -> Result<(), SagaDeliveryError> {
        let partition = self.partition;
        let reached = self.resolved_position().await.map_err(|hold| {
            hold.report(&self.delivery_id, partition);
            SagaDeliveryError::NotRecorded {
                partition,
                to: msg.to,
                detail: "the progress recorded for this partition could not be read".to_owned(),
            }
        })?;

        // Backwards is not a weaker form of the same operation: it would
        // redeliver a stretch this partition has finished with, and a
        // forward-only backend would refuse it. Refusing it here is what keeps
        // the operation meaning one thing in every deployment.
        if msg.to <= reached.cursor {
            return Err(SagaDeliveryError::NotForward {
                partition,
                to: msg.to,
                reached: reached.cursor,
            });
        }

        self.offsets
            .save(&self.scope, msg.to, None)
            .await
            .map_err(|failure| SagaDeliveryError::NotRecorded {
                partition,
                to: msg.to,
                detail: failure.to_string(),
            })?;

        self.position = Some(Position {
            cursor: msg.to,
            durable: msg.to,
        });
        self.consecutive_holds = 0;
        self.resume_at = None;

        tracing::warn!(
            partition,
            delivery_id = self.delivery_id.as_str(),
            global_sequence = msg.to,
            "saga delivery progress was moved on by an operator: the deliveries in the stretch \
             passed over are not made",
        );

        Ok(())
    }
}

/// Drives a partition on a fixed cadence.
///
/// Sleep first, then poll: a partition spawned over a journal it has already
/// finished with does not read it before the wiring that started it has
/// returned.
pub struct PartitionDriver<S: Saga, Cs: OffsetStore> {
    interval: Duration,
    _process: PhantomData<fn() -> (S, Cs)>,
}

impl<S: Saga, Cs: OffsetStore> PartitionDriver<S, Cs> {
    pub(crate) fn new(interval: Duration) -> Self {
        Self {
            interval,
            _process: PhantomData,
        }
    }
}

impl<S, Cs> Driver<SagaDeliveryPartition<S, Cs>> for PartitionDriver<S, Cs>
where
    S: Saga,
    Cs: OffsetStore + 'static,
{
    type Event = ();

    fn next(&mut self) -> impl Future<Output = Option<Self::Event>> + Send {
        let interval = self.interval;
        async move {
            tokio::time::sleep(interval).await;
            Some(())
        }
    }

    async fn apply(
        &mut self,
        state: &mut SagaDeliveryPartition<S, Cs>,
        _ctx: &mut ProcessContext<SagaDeliveryPartition<S, Cs>>,
        _ev: (),
    ) -> Result<(), HandlerError> {
        // A panic in a store a partition was handed is reported as a handler
        // failure, so supervision rebuilds the partition — which is also what
        // has it read its recorded progress again.
        match AssertUnwindSafe(state.tick()).catch_unwind().await {
            Ok(()) => Ok(()),
            Err(_panic) => {
                tracing::warn!(
                    partition = state.partition,
                    delivery_id = state.delivery_id.as_str(),
                    "a saga delivery partition panicked; supervision will restart it",
                );
                Err(HandlerError)
            }
        }
    }

    /// A partition is driven by its own cadence, so there is no idleness for a
    /// timeout to read: a quiet journal is a host with nothing to do, not one
    /// to stop.
    fn supports_idle_timeout(&self) -> bool {
        false
    }
}
