//! Where a delivery host is wired.

use std::marker::PhantomData;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use nitinol_eventsource::system::{EventSourceSystem, Hosted, SystemHandle, Unhosted};
use nitinol_eventsource::Event;
use nitinol_persistence::provider::{DeliveryCheckpointSlot, PersistenceProvider, Store};
use nitinol_persistence::store::{CheckpointStore, EventStore};
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
const RESTART_MAX_RETRIES: u32 = 5;

/// The window the retries above are counted in.
const RESTART_WITHIN: Duration = Duration::from_secs(60);

/// Wiring for a delivery host over the saga type `S`.
///
/// A delivery host subscribes to the decisions every instance of `S` has
/// persisted, turns each into the commands [`Saga::dispatch`] says it implies,
/// and delivers them to the aggregates they are addressed to. Polling, the
/// cursor, the recorded progress, holding on a failure and backing off are all
/// the host's; what the saga author writes is the mapping.
///
/// This type is where the delivery guarantee is written down. Everything else
/// in the crate that needs it links here rather than restating it.
///
/// # Delivery is at-least-once, and the checkpoint is where that comes from
///
/// A host is a pull consumer of the saga's own stream, and the only thing it
/// keeps is how far it has finished: the checkpoint. Delivering and recording
/// are two writes to two stores, and nothing makes them one — so the order is
/// fixed and the window is the consequence. A record's deliveries are carried
/// out first and the checkpoint moves past it only once every one of them has
/// landed. A host that died in between resumes from the checkpoint it last
/// recorded and carries the same deliveries again.
///
/// That window is the whole of why delivery is at-least-once, and it is why
/// there is no mode to pick: at-most-once would mean moving the checkpoint
/// before the command is sent, which loses a delivery to the same crash
/// instead of repeating one; exactly-once would need the receiver's write and
/// the checkpoint in one transaction across two stores, which nitinol does not
/// offer and does not pretend to. Receivers are therefore written to recognise
/// a repeat, from the operation id the domain put on the command — the saga
/// author's own data, since the framework attaches no envelope of its own.
///
/// A repeat is not only a crash's doing. Changing the partition count
/// reassigns stream keys across checkpoints (below), and
/// [`SagaDeliveryProxy::advance_checkpoint`](crate::SagaDeliveryProxy) is an
/// operator moving one by hand.
///
/// # Holding, and what it means
///
/// A delivery the receiver did not carry out is not passed over. The partition
/// holds: it stops advancing its checkpoint, retries the same record, and says
/// so through `tracing`. Nothing is skipped for failing, however permanently —
/// a command silently dropped because its target refused it is a decision the
/// saga made and nobody carried out, with nothing left to say so.
///
/// What a hold costs is the records behind it on the same partition: they wait.
/// [`with_partitions`](Self::with_partitions) is how far that reaches — a hold
/// is confined to the instances of one partition, each with its own
/// checkpoint — and
/// [`advance_checkpoint`](crate::SagaDeliveryProxy::advance_checkpoint) is the
/// operator's way past one, which is a decision to abandon that delivery and is
/// written down as such.
///
/// A hold is not a failure of the saga: the instance that decided has long
/// since moved on, and its own stream already holds the decision. Stopping the
/// saga for it would strip every other decision of its delivery.
///
/// # Two layers of failure, and only one of them is the host's
///
/// *Transport* failure is the host's: the target could not be reached, its
/// mailbox was closed, the ask timed out, the receiver answered an error. The
/// host retries it, holds while it persists, and reports it. A saga is not
/// notified, and there is no hook to register for one — a saga that reacted to
/// transport would be reacting to the framework's own plumbing.
///
/// *Business* failure is the domain's, and reaches the saga the only way a
/// domain fact can: as an input. The addressed aggregate records what it
/// decided, the saga observes that event through the source it declared for it,
/// and "the answer never came" is the saga's own deadline firing — see
/// [`Deadline`](crate::Deadline). So a compensation is written in the one
/// `match` of [`Saga::handle`](crate::Saga::handle), beside the reaction to the
/// answer that did arrive.
///
/// The two are kept apart because conflating them makes both unreadable: a
/// refused command and an unreachable node are the same signal to a saga that
/// hears about transport, and neither can be acted on correctly from there.
///
/// # Changing the partition count
///
/// The partition a stream key falls in is derived from the count, so changing
/// the count reassigns keys. A partition's recorded position covers the set of
/// streams that used to map to it, and after the change it covers a different
/// set: records already delivered under the old assignment are read again under
/// the new one, and records not yet delivered may move to a partition whose
/// recorded position is already past them.
///
/// What that costs is redelivery, which the receivers' idempotency absorbs —
/// the same thing a crash costs. What it is **not** safe against is delivery
/// being skipped, so a count is changed with every partition drained: stop the
/// host, let nothing be left unfinished, then start it with the new count.
/// Changing it under a running host, or restarting with a new count while a
/// partition is held, can carry a position past records nothing then delivers.
///
/// # Where it starts
///
/// `H` is the same typestate the other props carry: wired from a store through
/// [`new`](Self::new) these are [`Unhosted`] and
/// [`spawn`](SagaDeliveryProps::spawn) names the system to deliver through,
/// and taken from
/// [`SagaDeliveryExt::saga_delivery_props`](crate::SagaDeliveryExt::saga_delivery_props)
/// they are [`Hosted`] and `spawn` names nothing.
pub struct SagaDeliveryProps<S: Saga, Cs: CheckpointStore, H = Unhosted> {
    journal: Arc<dyn EventStore>,
    checkpoints: Arc<Cs>,
    delivery_id: Option<SagaDeliveryId>,
    partitions: NonZeroUsize,
    poll_interval: Duration,
    host: H,
    _saga: PhantomData<fn() -> S>,
}

impl<S, Cs> SagaDeliveryProps<S, Cs, Unhosted>
where
    S: Saga,
    Cs: CheckpointStore + 'static,
{
    /// Begin wiring a host over the two slots `persistence` distributes.
    ///
    /// The journal is the store the saga's own decisions were persisted to,
    /// which is the host's whole input, and the delivery-checkpoint slot is
    /// where its progress is recorded. A provider may have configured that slot
    /// with the same store a projection records to: the keys cannot collide, so
    /// the table is shared — and replacing the store the projection slot is
    /// configured with still leaves this one alone.
    ///
    /// A provider supplying no delivery slot is refused here rather than when
    /// the host first tries to record where it got to; the typestate doctest on
    /// [`SagaDeliveryExt::saga_delivery_props`](crate::SagaDeliveryExt::saga_delivery_props)
    /// fixes that, and a deployment with a system reaches this wiring through
    /// it.
    pub fn new<St>(persistence: &PersistenceProvider<St>) -> Self
    where
        St: DeliveryCheckpointSlot<DeliveryCheckpoints = Cs>,
    {
        Self {
            journal: persistence.event_store(),
            checkpoints: persistence.delivery_checkpoints(),
            delivery_id: None,
            partitions: DEFAULT_PARTITIONS,
            poll_interval: DEFAULT_POLL_INTERVAL,
            host: Unhosted,
            _saga: PhantomData,
        }
    }

    /// Carry `host` as the system these partitions start on and deliver
    /// through.
    pub(crate) fn hosted(self, host: SystemHandle) -> SagaDeliveryProps<S, Cs, Hosted> {
        SagaDeliveryProps {
            journal: self.journal,
            checkpoints: self.checkpoints,
            delivery_id: self.delivery_id,
            partitions: self.partitions,
            poll_interval: self.poll_interval,
            host: Hosted::new(host),
            _saga: PhantomData,
        }
    }
}

impl<S, Cs, H> SagaDeliveryProps<S, Cs, H>
where
    S: Saga,
    Cs: CheckpointStore + 'static,
{
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
    /// deployment wants that is not something the framework can know.
    pub fn with_poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = interval;
        self
    }

    /// Start the host on `host`, and answer the handle an operator reaches it
    /// through.
    ///
    /// The partitions are top-level processes of `host`'s process system, and
    /// they deliver through it: a target is reached by the same reference any
    /// other caller would resolve it with, so a receiving aggregate needs
    /// nothing of the host's on it.
    async fn start_on(self, host: SystemHandle) -> SagaDeliveryProxy<S> {
        let delivery_id = self.delivery_id.unwrap_or_else(|| {
            // The key is persisted, so what it is derived from has to be
            // something the type itself fixes. The declared event type is: one
            // per saga type, and the same text in every build.
            SagaDeliveryId::new(<S::Event as Event>::EVENT_TYPE.to_path().to_string())
        });

        let mut handles: Vec<Box<dyn PartitionHandle>> = Vec::with_capacity(self.partitions.get());

        for partition in 0..self.partitions.get() {
            let delivery_id = delivery_id.clone();
            let journal = Arc::clone(&self.journal);
            let checkpoints = Arc::clone(&self.checkpoints);
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
                    Arc::clone(&journal),
                    Arc::clone(&checkpoints),
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

impl<S, Cs> SagaDeliveryProps<S, Cs, Unhosted>
where
    S: Saga,
    Cs: CheckpointStore + 'static,
{
    /// Start the host, delivering through `system`, and answer the handle an
    /// operator reaches it through.
    ///
    /// `system` is any system, whatever slots its store holds — a host is
    /// wired from the supplier that holds its journal and its progress, and
    /// delivers through whichever system the receiving aggregates resolve on.
    /// The handle the partitions keep has the store's type erased, so the
    /// mapping a saga author wrote names one system type and the partitions
    /// stay one type whatever a deployment assembled.
    pub async fn spawn<St: Store>(self, system: &EventSourceSystem<St>) -> SagaDeliveryProxy<S> {
        let host = system.erase_store();
        self.start_on(host).await
    }
}

impl<S, Cs> SagaDeliveryProps<S, Cs, Hosted>
where
    S: Saga,
    Cs: CheckpointStore + 'static,
{
    /// Start the host on the system these props came from.
    ///
    /// Nothing is named here: the journal, the delivery slot and the system
    /// the deliveries are carried out on are all that system's.
    pub async fn spawn(self) -> SagaDeliveryProxy<S> {
        let host = self.host.system().clone();
        self.start_on(host).await
    }
}
