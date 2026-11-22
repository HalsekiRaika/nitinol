use std::sync::Arc;

use bytes::Bytes;
use nitinol_persistence::store::{DeliveryMode, OffsetScope, OffsetStore};
use nitinol_persistence::{AggregateId, LoadedEvent};

use crate::projection::ProjectionId;
use nitinol_runtime::process::{Process, ProcessContext, Receive};

use crate::durable_stream::{PolledBatch, SettledThrough};
use crate::error::ProjectionNotSettled;
use crate::projection::activator::Progress;
use crate::projection::delivery::{self, Batch, BatchOutcome, BatchRecord, ProjectionIdentity};
use crate::projection::handler::EventTypeHandler;
use crate::projection::tx_provider::ErasedTxProvider;

/// Specifies which sequence dimension drives offset advancement.
#[derive(Clone)]
pub(crate) enum CatchupOrigin {
    /// Use the per-aggregate `sequence` (stream-scoped cursor).
    Aggregate(AggregateId),
    /// Use the cross-aggregate `global_sequence`.
    Global,
}

/// What a record to be projected is handed to its handler as.
type Projection<P, Tx> = (Arc<dyn EventTypeHandler<P, Tx>>, Bytes);

/// The running projector: it receives what each read of its journal returned
/// as one batch, and settles that batch as one unit under its delivery mode.
///
/// The poller that reads for it is its sibling under the projection
/// activator, not its child: a projector stopping on its idle timeout waits for
/// its children, and a poller of its own could be waiting on it for an answer
/// at that very moment.
pub(crate) struct ProjectorProcess<P, Cs, Tx = ()> {
    pub(crate) projector: P,
    pub(crate) projection_id: ProjectionId,
    /// Built once, when this projector is spawned: the key its position is
    /// filed under never changes while it runs.
    pub(crate) offset_scope: OffsetScope,
    pub(crate) offset_store: Cs,
    pub(crate) delivery_mode: DeliveryMode,
    pub(crate) catchup_origin: CatchupOrigin,
    pub(crate) handlers: Vec<Arc<dyn EventTypeHandler<P, Tx>>>,
    pub(crate) tx_provider: Option<Arc<dyn ErasedTxProvider<Tx> + Send + Sync>>,
    pub(crate) offset_sequence: u64,
    /// How far through the journal, in global positions, this projection has
    /// settled and what it is known to be owed, as the activator weighs it when
    /// the projector stops.
    pub(crate) progress: Progress,
}

impl<P, Cs, Tx> Process for ProjectorProcess<P, Cs, Tx>
where
    P: Send + Sync + 'static,
    Cs: OffsetStore<Tx = Tx> + Send + Sync + 'static,
    Tx: Send + 'static,
{
}

impl<P, Cs, Tx> ProjectorProcess<P, Cs, Tx>
where
    P: Send + Sync + 'static,
    Tx: Send + 'static,
{
    /// Where `loaded` stands in the dimension this projector's offset is kept
    /// in.
    fn position_of(&self, loaded: &LoadedEvent) -> u64 {
        match &self.catchup_origin {
            CatchupOrigin::Aggregate(_) => loaded.sequence,
            CatchupOrigin::Global => loaded.global_sequence,
        }
    }

    /// Whether `loaded` is owed a handler's run, and which one.
    fn classify(&self, loaded: LoadedEvent) -> BatchRecord<Projection<P, Tx>> {
        let position = self.position_of(&loaded);

        // Already applied by this or an earlier incarnation.  Nothing is owed
        // for it, and reporting it as unsettled would hold the cursor on a
        // redelivery that can never produce anything new.
        if position <= self.offset_sequence {
            return BatchRecord::Skipped { position };
        }

        match self
            .handlers
            .iter()
            .find(|h| h.event_type() == loaded.event_type)
        {
            Some(h) => BatchRecord::ToProject {
                position,
                item: (Arc::clone(h), loaded.payload),
            },
            None => {
                // No handler is registered for this type, so there is no
                // application to fail and nothing a redelivery would change.
                // Holding the cursor here would stall every event behind it.
                tracing::debug!(
                    event_type = ?loaded.event_type,
                    "skipping unregistered event type",
                );
                BatchRecord::Skipped { position }
            }
        }
    }
}

/// A record of a batch as the activator weighs it.
struct TrackedRecord {
    /// In the dimension the projector's offset is kept in.
    position: u64,
    global: u64,
    /// Whether a handler is to run for it.
    owed: bool,
}

impl<P, Cs, Tx> ProjectorProcess<P, Cs, Tx> {
    /// Tell the activator how far `records` settled, and which of them stay
    /// owed: all of a batch that did not settle, the part behind `settled_through`
    /// of one that settled in part.
    fn report_progress(&self, records: &[TrackedRecord], settled_through: Option<u64>) {
        let settled =
            |record: &&TrackedRecord| settled_through.is_some_and(|t| record.position <= t);
        if let Some(global) = records.iter().filter(settled).map(|r| r.global).max() {
            self.progress.settle_through(global);
        }
        let owed = records
            .iter()
            .filter(|r| r.owed && !settled(r))
            .map(|r| r.global)
            .max();
        if let Some(global) = owed {
            self.progress.learn_of(global);
        }
    }
}

impl<P, Cs, Tx> Receive<PolledBatch<LoadedEvent>> for ProjectorProcess<P, Cs, Tx>
where
    P: Send + Sync + 'static,
    Cs: OffsetStore<Tx = Tx> + Send + Sync + 'static,
    Tx: Send + 'static,
{
    /// How far the batch settled; the poller moves the subscription cursor
    /// there, so the records behind a failure are redelivered.
    type Response = SettledThrough;
    /// Returning `Err` stops the poller (via `ask()`) from advancing the
    /// subscription cursor at all, so a batch this projector could not settle
    /// any of is redelivered from its first record rather than skipped for
    /// good.
    type Error = ProjectionNotSettled;

    async fn recv(
        &mut self,
        batch: PolledBatch<LoadedEvent>,
        _ctx: &mut ProcessContext<Self>,
    ) -> Result<SettledThrough, ProjectionNotSettled> {
        // The poller observed each record in the dimension its cursor was
        // built for from this projector's own origin; the position is read off
        // the record here, in the dimension this projector's offset is kept in.
        // Each record's global position is kept alongside, because what the
        // activator weighs against is global whatever this projector's origin.
        let mut tracked = Vec::with_capacity(batch.records.len());
        let records = batch
            .records
            .into_iter()
            .map(|(_observed, loaded)| {
                let (position, global) = (self.position_of(&loaded), loaded.global_sequence);
                let record = self.classify(loaded);
                tracked.push(TrackedRecord {
                    position,
                    global,
                    owed: matches!(record, BatchRecord::ToProject { .. }),
                });
                record
            })
            .collect();

        let outcome = delivery::apply_batch(
            self.delivery_mode,
            &self.offset_store,
            ProjectionIdentity {
                id: &self.projection_id,
                scope: &self.offset_scope,
            },
            Batch {
                records,
                last_observed: batch.last_observed,
            },
            self.tx_provider.as_deref(),
            &mut self.projector,
            |projector, (handler, payload), ctx| {
                Box::pin(async move {
                    tokio::task::yield_now().await;
                    handler.handle(projector, payload, ctx).await
                })
            },
        )
        .await;

        // Both the offset this projector carries forward and the answer the
        // poller reads come from this one outcome.  Deciding them separately is
        // how the cursor came to move past events the offset had not.
        let settled_through = match outcome {
            BatchOutcome::Settled { through } => Some(through),
            BatchOutcome::Unsettled => None,
        };
        // Before the answer, so that by the time this projector can stop, the
        // activator reads what it settled and what it still owes: the poller
        // holds its cursor there and reads those records again only at its
        // next tick, which an idle timeout may precede.
        self.report_progress(&tracked, settled_through);

        // Both the offset this projector carries forward and the answer the
        // poller reads come from this one outcome.  Deciding them separately is
        // how the cursor came to move past events the offset had not.
        match settled_through {
            Some(through) => {
                // A batch of nothing but records the offset already covers
                // settles through a position at or below it; the offset never
                // moves back.
                self.offset_sequence = self.offset_sequence.max(through);
                Ok(SettledThrough(through))
            }
            None => Err(ProjectionNotSettled),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::marker::PhantomData;
    use std::sync::atomic::{AtomicU64, Ordering};

    use async_trait::async_trait;
    use nitinol_persistence::store::InMemoryOffsetStore;
    use nitinol_persistence::{EventType, Family, TypeName};
    use nitinol_runtime::{ProcessSystem, Props};

    use crate::codec::Codec;
    use crate::projection::handler::ConcreteHandler;
    use crate::projection::{ProjectionContext, Projector};
    use crate::Event;

    #[derive(Clone)]
    struct Noted;

    struct UnitCodec;

    impl Codec<Noted> for UnitCodec {
        type Error = std::convert::Infallible;

        fn encode(_event: &Noted) -> Result<Bytes, Self::Error> {
            Ok(Bytes::new())
        }

        fn decode(_payload: &[u8]) -> Result<Noted, Self::Error> {
            Ok(Noted)
        }
    }

    impl Event for Noted {
        const EVENT_TYPE: EventType = EventType::new(
            Family::new("projector.process.test"),
            TypeName::new("Noted"),
        );

        type Codec = UnitCodec;
    }

    struct CountingProjector {
        projected: Arc<AtomicU64>,
    }

    #[async_trait]
    impl Projector<Noted> for CountingProjector {
        type Error = std::convert::Infallible;

        async fn project(
            &mut self,
            _event: Noted,
            _ctx: &mut ProjectionContext<'_, ()>,
        ) -> Result<(), Self::Error> {
            self.projected.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    /// An event at or below the projector's offset was applied by an earlier
    /// incarnation. Accepting it without projecting it is what lets the cursor
    /// move past a redelivery; reporting it as a failure would stall the
    /// stream.
    #[tokio::test]
    async fn already_processed_event_is_accepted_without_projecting() {
        let stream = AggregateId::new("duplicate-agg");
        let projection_id = ProjectionId::new("duplicate");
        let projected = Arc::new(AtomicU64::new(0));
        let handler: Arc<dyn EventTypeHandler<CountingProjector, ()>> =
            Arc::new(ConcreteHandler::<CountingProjector, Noted> {
                _phantom: PhantomData,
            });
        let projector = {
            let projected = Arc::clone(&projected);
            let stream = stream.clone();
            Props::new(move || ProjectorProcess {
                projector: CountingProjector {
                    projected: Arc::clone(&projected),
                },
                offset_scope: OffsetScope::from(&projection_id),
                projection_id: projection_id.clone(),
                offset_store: InMemoryOffsetStore::default(),
                delivery_mode: DeliveryMode::AtLeastOnce,
                catchup_origin: CatchupOrigin::Aggregate(stream.clone()),
                handlers: vec![Arc::clone(&handler)],
                tx_provider: None,
                offset_sequence: 5,
                progress: Progress::default(),
            })
        };
        let projector = ProcessSystem::new().await.spawn(projector).await;

        let already_processed = LoadedEvent {
            stream_key: stream.as_str().to_owned(),
            sequence: 3,
            global_sequence: 3,
            event_type: Noted::EVENT_TYPE,
            payload: Bytes::new(),
            occurred_at: jiff::Timestamp::now(),
        };
        let accepted = projector
            .ask(PolledBatch {
                records: vec![(3, already_processed)],
                last_observed: 3,
            })
            .await;

        match accepted {
            Ok(SettledThrough(through)) => assert_eq!(
                through, 3,
                "an event at or below the offset must be settled so the cursor can move past it"
            ),
            Err(e) => panic!(
                "an event at or below the offset must be accepted so the cursor can move past it, \
                 got {e:?}"
            ),
        }
        assert_eq!(
            projected.load(Ordering::SeqCst),
            0,
            "an already-processed event must not be projected again"
        );
    }
}
