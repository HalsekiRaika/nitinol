//! The handle a deployment keeps on a running delivery host.

use std::marker::PhantomData;

use futures_core::future::BoxFuture;

use nitinol_persistence::store::OffsetStore;
use nitinol_runtime::process::ProcessProxy;

use crate::delivery::error::SagaDeliveryError;
use crate::delivery::id::SagaDeliveryId;
use crate::delivery::process::{AdvanceOffset, SagaDeliveryPartition};
use crate::saga::Saga;

/// One partition, reachable without naming the offset store it was wired
/// with.
///
/// A handle names a running partition of this host, and which store that
/// partition records to is the wiring's business rather than the handle
/// holder's.
pub(crate) trait PartitionHandle: Send + Sync {
    fn advance(&self, partition: usize, to: u64) -> BoxFuture<'_, Result<(), SagaDeliveryError>>;
}

impl<S, Cs> PartitionHandle for ProcessProxy<SagaDeliveryPartition<S, Cs>>
where
    S: Saga,
    Cs: OffsetStore + 'static,
{
    fn advance(&self, partition: usize, to: u64) -> BoxFuture<'_, Result<(), SagaDeliveryError>> {
        Box::pin(async move {
            match self.ask(AdvanceOffset { to }).await {
                Ok(()) => Ok(()),
                Err(nitinol_runtime::error::AskError::Handler(refusal)) => Err(refusal),
                Err(_undelivered) => Err(SagaDeliveryError::Unreachable { partition }),
            }
        })
    }
}

/// A running delivery host for `S`.
///
/// Dropping this does not stop the host: the partitions are processes of the
/// system they were spawned on, and this is how an operator reaches them.
pub struct SagaDeliveryProxy<S: Saga> {
    delivery_id: SagaDeliveryId,
    partitions: Vec<Box<dyn PartitionHandle>>,
    _saga: PhantomData<fn() -> S>,
}

impl<S: Saga> SagaDeliveryProxy<S> {
    pub(crate) fn new(
        delivery_id: SagaDeliveryId,
        partitions: Vec<Box<dyn PartitionHandle>>,
    ) -> Self {
        Self {
            delivery_id,
            partitions,
            _saga: PhantomData,
        }
    }

    /// What this host's recorded progress is filed under.
    ///
    /// The one the wiring named, or the one derived from the saga's declared
    /// event type when it named none.
    pub fn delivery_id(&self) -> &SagaDeliveryId {
        &self.delivery_id
    }

    /// Declare that partition `partition` will not deliver anything at or
    /// below `to`, and have it resume above it.
    ///
    /// # What this is for
    ///
    /// A delivery the receiver refuses for good is held rather than passed
    /// over, because a loop cannot tell a command that should never have been
    /// sent from one whose target has yet to reach the state that would accept
    /// it. Everything behind the held record waits with it. This is the way
    /// out, and it is deliberately a person's to take.
    ///
    /// # What it means
    ///
    /// The recorded position is a cache of progress and can be rebuilt by
    /// reading from the beginning — but this is not a rebuild. It says the
    /// commands the stretch being passed over implied **will not be
    /// delivered**: whatever the saga decided there does not reach its target,
    /// now or later. Whether the business can live with that, and whether
    /// anything has to be put right by hand, is the judgement being made here.
    /// Nothing in the framework can make it.
    ///
    /// The host is not restarted and nothing else pauses: the partition named
    /// is reading from the new position by the time this returns, and the other
    /// partitions never stopped.
    ///
    /// # What it refuses
    ///
    /// A move that is not strictly forward, and a move to a partition this host
    /// does not have. A refusal changes nothing — see [`SagaDeliveryError`].
    pub async fn advance_offset(&self, partition: usize, to: u64) -> Result<(), SagaDeliveryError> {
        let Some(handle) = self.partitions.get(partition) else {
            return Err(SagaDeliveryError::NoSuchPartition {
                partition,
                partitions: self.partitions.len(),
            });
        };
        handle.advance(partition, to).await
    }
}
