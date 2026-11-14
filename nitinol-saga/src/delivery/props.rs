//! Where a delivery host is wired.

use std::marker::PhantomData;
use std::num::NonZeroUsize;
use std::time::Duration;

use nitinol_eventsource::system::ContingentSystem;
use nitinol_eventsource::Event;
use nitinol_persistence::store::{Journal, OffsetStore};
use nitinol_runtime::process::SupervisionStrategy;
use nitinol_runtime::{IdleTimeout, Props};

use crate::delivery::id::SagaDeliveryId;
use crate::delivery::process::{PartitionDriver, SagaDeliveryPartition};
use crate::delivery::proxy::{PartitionHandle, SagaDeliveryProxy};
use crate::saga::Saga;

/// How often a partition asks its store for what is new, unless the wiring
/// says otherwise.
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// One partition, unless the wiring says otherwise.
///
/// A single consumer serialises every instance of the saga type behind the
/// slowest delivery, which is why anything with throughput to lose names a
/// number here.
const DEFAULT_PARTITIONS: NonZeroUsize = NonZeroUsize::MIN;

/// How many times a panicking partition is rebuilt before it is left stopped.
///
/// Shared with the deadline index's partitions, which are rebuilt on the same
/// terms: both are pull consumers of the journal that lose nothing by being
/// rebuilt.
pub(crate) const RESTART_MAX_RETRIES: u32 = 5;

/// The window the retries above are counted in.
pub(crate) const RESTART_WITHIN: Duration = Duration::from_secs(60);

/// Wiring for a delivery host over the saga type `S`.
///
/// A delivery host reads the decisions every instance of `S` has persisted,
/// turns each into the commands [`Saga::dispatch`] says it implies, and
/// delivers them to the aggregates they are addressed to. See the crate's
/// [mental model](crate#mental-model).
///
/// - Delivery is at-least-once: a record's offset moves past it only once every
///   delivery it implied has landed, so a host that dies in between delivers
///   them again. Receivers recognise a repeat from the operation id the domain
///   put on the command.
/// - A delivery the receiver did not carry out holds its partition: the same
///   record is retried and reported through `tracing`, and the records behind
///   it on that partition wait. Nothing is skipped for failing, and the saga
///   keeps running; [`advance_offset`](crate::SagaDeliveryProxy::advance_offset)
///   is the operator's way past a hold.
/// - A *transport* failure is the host's to retry, hold and report; the saga is
///   not notified. A *business* failure reaches the saga as an input: the event
///   the addressed aggregate recorded, or its own [`Deadline`](crate::Deadline)
///   firing when no answer came.
/// - Changing the partition count reassigns stream keys to offsets, so records
///   are read again and one not yet delivered can fall behind an offset already
///   past it. Change it only with every partition drained: stop the host, let
///   nothing be left unfinished, then start it with the new count.
/// - A host is started only by [`SagaManagerProps`](crate::SagaManagerProps),
///   which names these settings; the journal and the saga-offset slot are the
///   system's.
pub struct SagaDeliveryProps<S: Saga> {
    delivery_id: Option<SagaDeliveryId>,
    partitions: NonZeroUsize,
    poll_interval: Duration,
    _saga: PhantomData<fn() -> S>,
}

impl<S: Saga> SagaDeliveryProps<S> {
    /// The settings a host starts with when the saga's builder names none.
    pub(crate) fn new() -> Self {
        Self {
            delivery_id: None,
            partitions: DEFAULT_PARTITIONS,
            poll_interval: DEFAULT_POLL_INTERVAL,
            _saga: PhantomData,
        }
    }

    /// Name this host, so its recorded progress is filed under `id`.
    ///
    /// Without it the identifier is derived from the event type `S` declares.
    /// That default is enough for one host over one saga type; a second
    /// deployment, or a migration running beside the first, names its own so
    /// the two keep separate progress.
    pub fn with_delivery_id(mut self, id: SagaDeliveryId) -> Self {
        self.delivery_id = Some(id);
        self
    }

    /// Spread the subscription over `partitions` independent processes.
    ///
    /// Each partition takes the instances whose stream key falls in it, and has
    /// its own recorded progress and its own holding. A delivery that cannot be
    /// made therefore holds the instances of one partition rather than the
    /// whole host — which is what bounds how far a single stuck receiver
    /// reaches.
    ///
    /// What the count costs to change is stated on this type.
    pub fn with_partitions(mut self, partitions: NonZeroUsize) -> Self {
        self.partitions = partitions;
        self
    }

    /// Ask the store no more often than every `interval`.
    ///
    /// A pull consumer trades latency for load on the store, and which way a
    /// deployment wants that is not something the framework can know. The
    /// latency floor and the store load an interval implies are worked out on
    /// [`DEFAULT_POLL_INTERVAL`](nitinol_eventsource::DEFAULT_POLL_INTERVAL).
    ///
    /// The first read does not wait for the interval: each partition, and the
    /// saga's deadline index, reads as soon as it is spawned — the same as a
    /// projector's poller, whose cadence is named on
    /// [`ProjectorProps::with_poll_interval`](nitinol_eventsource::ProjectorProps::with_poll_interval).
    pub fn with_poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = interval;
        self
    }

    /// The partition count this host is wired with, which the saga's deadline
    /// index is spread over too.
    pub(crate) fn partitions(&self) -> NonZeroUsize {
        self.partitions
    }

    /// The cadence this host asks the journal at, which the saga's deadline
    /// index asks it at too.
    pub(crate) fn poll_interval(&self) -> Duration {
        self.poll_interval
    }

    /// Start the host over `journal` and `offsets`, delivering through `host`,
    /// and answer the handle an operator reaches it through.
    ///
    /// The two stores arrive as values from the saga's own spawn, which resolved
    /// them from the system's supplier: this is the only way a host is started,
    /// so there is no second place a destination could be named.
    ///
    /// The partitions are top-level processes of `host`'s process system, and
    /// they deliver through it: a target is reached by the same reference any
    /// other caller would resolve it with, so a receiving aggregate needs
    /// nothing of the host's on it.
    pub(crate) async fn start_on<Cs>(
        self,
        journal: Journal,
        offsets: Cs,
        host: ContingentSystem,
    ) -> SagaDeliveryProxy<S>
    where
        Cs: OffsetStore + Clone + 'static,
    {
        let delivery_id = self.delivery_id.unwrap_or_else(|| {
            // The key is persisted, so what it is derived from has to be
            // something the type itself fixes. The declared event type is: one
            // per saga type, and the same text in every build.
            SagaDeliveryId::new(<S::Event as Event>::EVENT_TYPE.to_path().to_string())
        });

        let mut handles: Vec<Box<dyn PartitionHandle>> = Vec::with_capacity(self.partitions.get());

        for partition in 0..self.partitions.get() {
            let delivery_id = delivery_id.clone();
            let journal = journal.clone();
            let offsets = offsets.clone();
            let system_for_partition = host.clone();
            let partitions = self.partitions;
            let poll_interval = self.poll_interval;

            let props = Props::new(move || {
                // Nothing of where the partition had got to is captured here:
                // a rebuilt partition reads its recorded progress again, so a
                // restart resumes from what the store holds then rather than
                // from what it held when the host was wired.
                SagaDeliveryPartition::<S, Cs>::new(
                    delivery_id.clone(),
                    partition,
                    partitions,
                    journal.clone(),
                    offsets.clone(),
                    system_for_partition.clone(),
                    poll_interval,
                )
            })
            // A partition holds a position and a hold, both of which it can
            // read back from the store, so rebuilding it loses nothing — and
            // rebuilding is what has it read the store again.
            .with_supervision_strategy(
                SupervisionStrategy::restart(RESTART_MAX_RETRIES, RESTART_WITHIN)
                    .expect("the restart window is a positive duration constant"),
            )
            // A quiet journal is a host with nothing to deliver, not one to
            // stop: there would be nothing left to revive it.
            .with_idle_timeout(IdleTimeout::Persistent)
            .with_driver(PartitionDriver::<S, Cs>::new(self.poll_interval));

            handles.push(Box::new(host.process_system().spawn(props).await));
        }

        SagaDeliveryProxy::new(delivery_id, handles)
    }
}
