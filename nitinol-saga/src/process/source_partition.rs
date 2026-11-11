//! One partition of one declared source of a saga manager: the loop that reads
//! the source, hands the manager the records of its partition, and records how
//! far everything it read is settled.

use std::marker::PhantomData;
use std::num::NonZeroUsize;
use std::panic::AssertUnwindSafe;
use std::time::Duration;

use futures_util::{FutureExt, StreamExt};

use nitinol_eventsource::{SequenceCursor, DEFAULT_POLL_INTERVAL};
use nitinol_persistence::store::{Journal, OffsetScope};
use nitinol_persistence::{AggregateId, EventType, LoadQuery, LoadedEvent};
use nitinol_runtime::error::HandlerError;
use nitinol_runtime::process::{
    Driver, Process, ProcessContext, ProcessProxy, SupervisionStrategy,
};
use nitinol_runtime::{IdleTimeout, Props};

use crate::consumed::PositionSource;
use crate::delivery::{partition_of, SagaDeliveryId, RESTART_MAX_RETRIES, RESTART_WITHIN};
use crate::process::manager::SagaManagerProcess;
use crate::process::manager_offsets::SagaManagerOffsets;
use crate::process::saga_process::{UpstreamMessage, UpstreamSettlement};
use crate::saga::Saga;
use crate::subscribe::{DecodeSource, SourceBinding, SubscribedSource};

/// The part of the framework a manager's offsets belong to.
///
/// Carries no `.`, which is what [`OffsetScope::framework_owned`] asks of a
/// part name, and differs from the delivery host's `saga-delivery`, which is
/// what lets both record in one table without a key of one ever being a key of
/// the other.
const SAGA_MANAGER_PART: &str = "saga-manager";

/// One declared source, resolved at the manager's spawn into what each of its
/// partitions reads with.
pub(crate) struct UpstreamSource<A> {
    event_type: EventType,
    /// What a consumed position names this source as, decided once here so
    /// every partition of the source names it identically.
    position_source: PositionSource,
    decode: DecodeSource<A>,
    /// The floor the [`Subscription`](crate::Subscription) named: the stream or
    /// the whole journal it reads, and the position at or below which nothing
    /// is ever delivered. Its dimension is the one the source's offset is kept
    /// in.
    floor: SequenceCursor,
    poll_interval: Duration,
}

impl<A> Clone for UpstreamSource<A> {
    fn clone(&self) -> Self {
        Self {
            event_type: self.event_type,
            position_source: self.position_source.clone(),
            decode: self.decode.clone(),
            floor: self.floor.clone(),
            poll_interval: self.poll_interval,
        }
    }
}

impl<A> UpstreamSource<A> {
    /// Resolve one declared source.
    ///
    /// A binding that named no interval polls at the one `nitinol-eventsource`
    /// owns, rather than at a value restated here.
    pub(crate) fn resolve(source: SubscribedSource<A>) -> Self
    where
        A: 'static,
    {
        let event_type = source.event_type();
        let (decode, binding) = source.into_parts();
        let SourceBinding {
            cursor,
            poll_interval,
        } = binding;
        Self {
            event_type,
            position_source: PositionSource::of(event_type),
            decode,
            floor: cursor,
            poll_interval: poll_interval.unwrap_or(DEFAULT_POLL_INTERVAL),
        }
    }

    /// The key partition `partition` of this source, under the host named
    /// `host`, records its progress under.
    ///
    /// Rendered as
    /// `OffsetScope::framework_owned("saga-manager", "{n}:{host}:{source}", partition)`,
    /// where `n` is the byte length of `host` in decimal and `source` is the
    /// canonical path of the source's declared event type.
    ///
    /// One-to-one in (host, source, partition). [`OffsetScope`] already
    /// renders (part, name, partition) injectively, so what is left is the
    /// name: neither a host identifier nor an event type path refuses `:`, so
    /// no separator alone could say where one ends. The length can: the digits
    /// up to the first `:` are `n` — a decimal number holds no `:` — the next
    /// `n` bytes are the host, whatever they contain, and what follows the `:`
    /// after them is the source path. Two different pairs therefore never
    /// render one name.
    ///
    /// The host is the delivery host's identifier, so a deployment that names
    /// its host names the manager's progress with it, and two hosts over one
    /// saga type keep apart on both halves.
    fn offset_scope(&self, host: &SagaDeliveryId, partition: usize) -> OffsetScope {
        let host = host.as_str();
        OffsetScope::framework_owned(
            SAGA_MANAGER_PART,
            &format!("{}:{host}:{}", host.len(), self.event_type.to_path()),
            partition,
        )
    }

    fn floor(&self) -> u64 {
        match &self.floor {
            SequenceCursor::Stream { after, .. } | SequenceCursor::Global { after } => *after,
        }
    }

    /// The read of every record of this source above `position`, the
    /// partition's cursor.
    ///
    /// A stream source names its key and nothing else, and a global source
    /// names nothing but the cursor: this source claims its own rows by
    /// event type as it reads them, so the store is asked for no type.
    fn query_above(&self, position: u64) -> LoadQuery {
        match &self.floor {
            SequenceCursor::Stream { key, .. } => {
                LoadQuery::by_stream(key).with_from_stream_sequence(position + 1)
            }
            SequenceCursor::Global { .. } => LoadQuery::from_global(position + 1),
        }
    }

    /// Where `loaded` stands in the dimension this source counts in.
    fn position_of(&self, loaded: &LoadedEvent) -> u64 {
        match &self.floor {
            SequenceCursor::Stream { .. } => loaded.sequence,
            SequenceCursor::Global { .. } => loaded.global_sequence,
        }
    }

    /// `loaded` as this source's variant of the alphabet, or `None` for a row
    /// this source does not claim or cannot read.
    ///
    /// A source claims the rows of its own event type and no others, which is
    /// what lets several sources read one stream key without reading each
    /// other's records.
    fn read(&self, loaded: &LoadedEvent) -> Option<A> {
        if loaded.event_type != self.event_type {
            return None;
        }
        match (self.decode)(&loaded.payload) {
            Ok(input) => Some(input),
            Err(e) => {
                // Reported and skipped, which is the whole of the handling:
                // a codec that refused these bytes once refuses them every
                // time, and with no typed input there is no instance to
                // attribute the failure to — so there is nothing a retry or
                // a held offset could recover.
                tracing::error!(
                    error = %e,
                    event_type = %self.event_type,
                    upstream_key = loaded.stream_key.as_str(),
                    upstream_sequence = loaded.sequence,
                    "saga could not decode an observed record; skipping it",
                );
                None
            }
        }
    }
}

/// Where a partition has got to, once it has read its recorded offset.
///
/// Absent until then, which is what a partition rebuilt by supervision starts
/// from again.
#[derive(Clone, Copy)]
struct Position {
    /// The last record this incarnation read and finished asking about, every
    /// record at or below which is settled. Reading stops at a record that is
    /// not, so this never moves past one.
    cursor: u64,
    /// The record the partition is stopped at because its instance left it
    /// unsettled, kept so that being stopped there is reported once rather
    /// than on every tick the same record is handed again.
    stalled_at: Option<u64>,
    /// The last offset the store has taken.
    durable: u64,
}

/// What a partition makes of one record it read.
enum Verdict {
    /// Owed nothing more: decided, settled by its consumed position, addressed
    /// to no instance or to another partition's, unreadable, or dropped by an
    /// ended instance.
    Settled,
    /// Handed to its instance, whose decision never became durable. The
    /// partition reads no further, so the same record is handed again on the
    /// next turn.
    Unsettled,
    /// Not answered at all; the partition reads no further, so the same record
    /// is handed again on the next turn.
    Held,
}

/// One partition of one declared source of a saga manager.
///
/// Every partition of a source reads the whole source from its own offset,
/// and hands the manager only the records whose instance falls in it, by
/// [`partition_of`] over the id [`Saga::correlate`] answers. The rest — another
/// partition's, and the ones no instance claims — are settled by owing them
/// nothing. A record has to be decoded and correlated before its partition is
/// known, so each partition decodes what it reads, and a partition held at a
/// record holds up the instances of its own partition alone.
pub(crate) struct SagaSourcePartition<S: Saga> {
    source: UpstreamSource<S::Subscribe>,
    partition: usize,
    partitions: NonZeroUsize,
    scope: OffsetScope,
    journal: Journal,
    offsets: SagaManagerOffsets,
    manager: ProcessProxy<SagaManagerProcess<S>>,
    position: Option<Position>,
}

impl<S: Saga> Clone for SagaSourcePartition<S> {
    fn clone(&self) -> Self {
        Self {
            source: self.source.clone(),
            partition: self.partition,
            partitions: self.partitions,
            scope: self.scope.clone(),
            journal: self.journal.clone(),
            offsets: self.offsets.clone(),
            manager: self.manager.clone(),
            position: self.position,
        }
    }
}

impl<S: Saga> SagaSourcePartition<S> {
    /// Partition `partition` of `partitions` of `source`, recording under the
    /// host named `host`, reading `journal` and handing its records to
    /// `manager`.
    pub(crate) fn new(
        source: UpstreamSource<S::Subscribe>,
        partition: usize,
        partitions: NonZeroUsize,
        host: &SagaDeliveryId,
        journal: Journal,
        offsets: SagaManagerOffsets,
        manager: ProcessProxy<SagaManagerProcess<S>>,
    ) -> Self {
        Self {
            scope: source.offset_scope(host, partition),
            source,
            partition,
            partitions,
            journal,
            offsets,
            manager,
            position: None,
        }
    }

    /// The props this partition is spawned from as a child of its manager.
    pub(crate) fn into_props(self) -> Props<Self, SourcePartitionDriver<S>> {
        let poll_interval = self.source.poll_interval;
        // Built before it has read anything, so nothing of where it got to is
        // captured here: a rebuilt partition reads its recorded offset again,
        // and resumes from what the store holds then.
        Props::new(move || self.clone())
            // A partition holds a position it can read back from the store, so
            // rebuilding it loses nothing — and rebuilding is what has it read
            // the store again.
            .with_supervision_strategy(
                SupervisionStrategy::restart(RESTART_MAX_RETRIES, RESTART_WITHIN)
                    .expect("the restart window is a positive duration constant"),
            )
            // A quiet source is a partition with nothing to read, not one to
            // stop: there would be nothing left to revive it.
            .with_idle_timeout(IdleTimeout::Persistent)
            .with_driver(SourcePartitionDriver::new(poll_interval))
    }

    /// One turn: read the records above where this partition got to, hand on
    /// the ones it owes, and record how far everything it read is settled.
    ///
    /// The offset is offered once, at the end of the turn, rather than per
    /// record: a save per record would multiply the writes by the length of
    /// every catch-up.
    async fn poll_once(&mut self) {
        let Some(mut position) = self.resolved_position().await else {
            return;
        };

        match self
            .journal
            .load(self.source.query_above(position.cursor))
            .await
        {
            Ok(mut stream) => {
                while let Some(item) = stream.next().await {
                    let loaded = match item {
                        Ok(loaded) => loaded,
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                source = %self.source.event_type,
                                partition = self.partition,
                                "saga manager partition could not read the rest of its source; \
                                 reading on from where it stopped on the next tick",
                            );
                            break;
                        }
                    };
                    let at = self.source.position_of(&loaded);
                    match self.judge(loaded).await {
                        Verdict::Settled => {
                            position.cursor = at;
                            if position.stalled_at == Some(at) {
                                position.stalled_at = None;
                            }
                        }
                        // Nothing past the record is read: a consumed position
                        // is the highest sequence an instance settled, so a
                        // later record of the same instance settling would
                        // cover this one and it would never reach `handle`
                        // again.
                        Verdict::Unsettled => {
                            if position.stalled_at != Some(at) {
                                position.stalled_at = Some(at);
                                tracing::warn!(
                                    source = %self.source.event_type,
                                    partition = self.partition,
                                    position = at,
                                    "saga manager partition is held at an unsettled record",
                                );
                            }
                            break;
                        }
                        Verdict::Held => break,
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    source = %self.source.event_type,
                    partition = self.partition,
                    "saga manager partition could not read its source; retrying on the next tick",
                );
            }
        }

        self.record(&mut position).await;
        self.position = Some(position);
    }

    /// Where this partition resumes from, reading the store the first time and
    /// after every rebuild; `None` for a turn whose offset could not be read,
    /// which then reads nothing.
    ///
    /// The higher of the floor and the recorded offset: the floor is never
    /// undercut, and a recorded offset is never read below. What the store
    /// holds is taken as already recorded even where the floor is above it, so
    /// nothing is saved until a record above both is settled.
    async fn resolved_position(&mut self) -> Option<Position> {
        if let Some(position) = self.position {
            return Some(position);
        }
        match self.offsets.load(&self.scope).await {
            Ok(stored) => {
                let floor = self.source.floor();
                let resume = stored.map_or(floor, |stored| stored.max(floor));
                tracing::info!(
                    source = %self.source.event_type,
                    partition = self.partition,
                    resume_position = resume,
                    floor,
                    stored_offset = ?stored,
                    "saga manager partition resumed",
                );
                let position = Position {
                    cursor: resume,
                    stalled_at: None,
                    durable: resume,
                };
                self.position = Some(position);
                Some(position)
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    source = %self.source.event_type,
                    partition = self.partition,
                    "saga manager partition could not read its recorded offset; \
                     reading nothing until the next tick",
                );
                None
            }
        }
    }

    /// Offer the store this partition's cursor — every record up to it is
    /// settled — as its offset, if it is ahead of the offset the store holds.
    ///
    /// Only ever a strict move forward, so a backend that takes none but
    /// forward moves and one that overwrites whatever it is handed answer this
    /// partition identically.
    async fn record(&self, position: &mut Position) {
        let settled = position.cursor;
        if settled <= position.durable {
            return;
        }
        match self.offsets.save(&self.scope, settled).await {
            Ok(()) => position.durable = settled,
            // The cursor is not wound back: the records up to it were handed
            // on, and reading them again would redeliver them for no gain. The
            // same position is offered again at the end of the next turn.
            Err(e) => tracing::warn!(
                error = %e,
                source = %self.source.event_type,
                partition = self.partition,
                position = settled,
                "saga manager could not record its progress; retrying on the next tick",
            ),
        }
    }

    /// What this partition makes of one record of its source.
    async fn judge(&self, loaded: LoadedEvent) -> Verdict {
        let Some(input) = self.source.read(&loaded) else {
            return Verdict::Settled;
        };
        // A record belonging to no instance is nobody's work, and a record of
        // another partition's instance is that partition's: standing at either
        // is right, where holding below records this partition will never
        // hand on would have it re-read the same tail forever.
        let Some(saga_id) = S::correlate(&input) else {
            return Verdict::Settled;
        };
        if partition_of(saga_id.as_str(), self.partitions) != self.partition {
            return Verdict::Settled;
        }

        let sequence = loaded.sequence;
        let message = UpstreamMessage {
            aggregate_id: AggregateId::new(loaded.stream_key),
            sequence,
            source: self.source.position_source.clone(),
            saga_id,
            input,
        };
        match self.manager.ask(message).await {
            Ok(UpstreamSettlement::Settled) => Verdict::Settled,
            Ok(UpstreamSettlement::Unsettled) => Verdict::Unsettled,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    source = %self.source.event_type,
                    partition = self.partition,
                    upstream_sequence = sequence,
                    "saga manager partition: the record was not settled; reading no further, \
                     so it is handed again on the next tick",
                );
                Verdict::Held
            }
        }
    }
}

impl<S: Saga> Process for SagaSourcePartition<S> {}

/// Drives a source partition on its source's cadence.
///
/// Poll first, then sleep, the cadence the delivery host's partitions keep: a
/// source partition reads as soon as it starts, and waits the interval between
/// one read and the next. The driver lives as long as its process: a
/// supervised restart rebuilds the partition but not this, so a rebuilt
/// partition waits the interval before its first read.
pub(crate) struct SourcePartitionDriver<S: Saga> {
    interval: Duration,
    has_ticked: bool,
    _process: PhantomData<fn() -> S>,
}

impl<S: Saga> SourcePartitionDriver<S> {
    fn new(interval: Duration) -> Self {
        Self {
            interval,
            has_ticked: false,
            _process: PhantomData,
        }
    }
}

impl<S: Saga> Driver<SagaSourcePartition<S>> for SourcePartitionDriver<S> {
    type Event = ();

    async fn next(&mut self) -> Option<Self::Event> {
        // The flag is read and set inside the future rather than when it is
        // made: the lifecycle loop drops a `next` future it never polled when
        // a system signal is waiting, and a flag set at creation would lose
        // the first turn to that.
        if self.has_ticked {
            tokio::time::sleep(self.interval).await;
        }
        self.has_ticked = true;
        Some(())
    }

    async fn apply(
        &mut self,
        state: &mut SagaSourcePartition<S>,
        _ctx: &mut ProcessContext<SagaSourcePartition<S>>,
        _ev: (),
    ) -> Result<(), HandlerError> {
        // A panic in a store a partition was handed is reported as a handler
        // failure, so supervision rebuilds the partition — which is also what
        // has it read its recorded offset again.
        match AssertUnwindSafe(state.poll_once()).catch_unwind().await {
            Ok(()) => Ok(()),
            Err(_panic) => {
                tracing::warn!(
                    source = %state.source.event_type,
                    partition = state.partition,
                    "a saga manager source partition panicked; supervision will restart it",
                );
                Err(HandlerError)
            }
        }
    }

    /// A partition is driven by its own cadence, so there is no idleness for a
    /// timeout to read: a quiet source is a partition with nothing to do, not
    /// one to stop.
    fn supports_idle_timeout(&self) -> bool {
        false
    }
}
