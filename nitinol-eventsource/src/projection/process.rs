use std::convert::Infallible;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use nitinol_persistence::error::OffsetError;
use nitinol_persistence::store::{DeliveryMode, Journal, OffsetScope, OffsetStore};
use nitinol_persistence::{AggregateId, LoadedEvent};
use nitinol_runtime::process::{Process, ProcessContext, ProcessProxy, Receive};

use crate::durable_stream::{
    BatchPollerProcess, DurableSubscription, PolledBatch, ReachedEnd, ReadNow, SequenceCursor,
    SettledThrough,
};
use crate::error::ProjectionNotSettled;
use crate::projection::activator::{
    CaughtUp, OffsetRestored, ProjectionActivator, ReadTakenUp, Settled,
};
use crate::projection::delivery::{self, Batch, BatchOutcome, BatchRecord, ProjectionIdentity};
use crate::projection::handler::EventTypeHandler;
use crate::projection::tx_provider::ErasedTxProvider;
use crate::projection::ProjectionId;

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
/// - It is a top-level process, and the poller that reads for it is its child.
/// - It reads its own offset as it starts, and builds the projector from the
///   producer only once that read succeeded. A read that fails is tried again
///   with every batch the poller hands over, and the batch is refused until
///   one succeeds; the poller reads from the journal's start meanwhile, and
///   what the offset turns out to cover is skipped.
/// - It tells the projection activator the offset it started from, how far
///   each batch settled, when its poller first found nothing to read, and when
///   it took up a request to read at once: what the activator weighs when this
///   projector stops, and when it asks for a read.
pub(crate) struct ProjectorProcess<P, Cs, Tx = ()> {
    pub(crate) projection_id: ProjectionId,
    /// Built once, when this projector is spawned: the key its position is
    /// filed under never changes while it runs.
    pub(crate) offset_scope: OffsetScope,
    pub(crate) journal: Journal,
    pub(crate) offset_store: Cs,
    pub(crate) delivery_mode: DeliveryMode,
    pub(crate) catchup_origin: CatchupOrigin,
    pub(crate) producer: Arc<dyn Fn() -> P + Send + Sync>,
    pub(crate) handlers: Vec<Arc<dyn EventTypeHandler<P, Tx>>>,
    pub(crate) tx_provider: Option<Arc<dyn ErasedTxProvider<Tx> + Send + Sync>>,
    pub(crate) poll_interval: Duration,
    pub(crate) batch_limit: NonZeroUsize,
    pub(crate) activator: ProcessProxy<ProjectionActivator>,
    /// `None` until the offset has been read.
    pub(crate) restored: Option<Restored<P>>,
    /// The child reading the journal; `None` until `on_start` spawned it.
    pub(crate) poller: Option<ProcessProxy<BatchPollerProcess<LoadedEvent, Self>>>,
}

/// The projector and the offset it was built at, which only exist together.
pub(crate) struct Restored<P> {
    projector: P,
    offset: u64,
}

impl<P, Cs, Tx> Process for ProjectorProcess<P, Cs, Tx>
where
    P: Send + Sync + 'static,
    Cs: OffsetStore<Tx = Tx> + Send + Sync + 'static,
    Tx: Send + 'static,
{
    async fn on_start(&mut self, ctx: &mut ProcessContext<Self>) {
        let after = match self.restore().await {
            Ok(offset) => offset,
            Err(error) => {
                tracing::warn!(
                    projection_id = ?self.projection_id,
                    error = %error,
                    "projector: loading the recorded offset failed; it is loaded again with \
                     the first batch read"
                );
                0
            }
        };
        let cursor = match &self.catchup_origin {
            CatchupOrigin::Aggregate(stream) => SequenceCursor::Stream {
                key: stream.as_str().to_owned(),
                after,
            },
            CatchupOrigin::Global => SequenceCursor::Global { after },
        };
        let poller = DurableSubscription::<LoadedEvent>::new(Some)
            .with_poll_interval(self.poll_interval)
            .spawn_batch_child(ctx, self.journal.clone(), cursor, self.batch_limit)
            .await;
        self.poller = Some(poller);
    }
}

impl<P, Cs, Tx> ProjectorProcess<P, Cs, Tx>
where
    P: Send + Sync + 'static,
    Cs: OffsetStore<Tx = Tx> + Send + Sync + 'static,
    Tx: Send + 'static,
{
    /// The offset this projector was built at, reading it — and building the
    /// projector — if that has not succeeded yet.
    async fn restore(&mut self) -> Result<u64, OffsetError> {
        if let Some(restored) = &self.restored {
            return Ok(restored.offset);
        }
        let offset = self
            .offset_store
            .load(&self.offset_scope)
            .await?
            .unwrap_or(0);
        self.restored = Some(Restored {
            projector: (self.producer)(),
            offset,
        });
        // What the offset covers was settled before this activation, which the
        // activator weighs a stop against: it has not seen it if this projection
        // was registered again after an earlier one reported it.
        self.report(OffsetRestored {
            id: self.projection_id.clone(),
            through: offset,
        })
        .await;
        Ok(offset)
    }

    /// Where `loaded` stands in the dimension this projector's offset is kept
    /// in.
    fn position_of(&self, loaded: &LoadedEvent) -> u64 {
        match &self.catchup_origin {
            CatchupOrigin::Aggregate(_) => loaded.sequence,
            CatchupOrigin::Global => loaded.global_sequence,
        }
    }

    /// Whether `loaded` is owed a handler's run past `offset`, and which one.
    fn classify(&self, offset: u64, loaded: LoadedEvent) -> BatchRecord<Projection<P, Tx>> {
        let position = self.position_of(&loaded);

        // Already applied by this or an earlier incarnation.  Nothing is owed
        // for it, and reporting it as unsettled would hold the cursor on a
        // redelivery that can never produce anything new.
        if position <= offset {
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

    /// Tell the activator `message`. The activator runs as long as the
    /// process system does, so a refusal only comes while that shuts down.
    async fn report<M>(&self, message: M)
    where
        ProjectionActivator: Receive<M>,
        M: Send + Sync + 'static,
    {
        if let Err(error) = self.activator.tell(message).await {
            tracing::debug!(
                projection_id = ?self.projection_id,
                error = %error,
                "projector: the projection activator has stopped; progress is not reported"
            );
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
        let offset = match self.restore().await {
            Ok(offset) => offset,
            Err(error) => {
                tracing::warn!(
                    projection_id = ?self.projection_id,
                    error = %error,
                    "projector: loading the recorded offset failed; the batch is read again"
                );
                return Err(ProjectionNotSettled);
            }
        };

        // The poller observed each record in the dimension its cursor was
        // built for from this projector's own origin; the position is read off
        // the record here, in the dimension this projector's offset is kept in.
        let records = batch
            .records
            .into_iter()
            .map(|(_observed, loaded)| self.classify(offset, loaded))
            .collect();

        let restored = self
            .restored
            .as_mut()
            .expect("restore() above answered the offset, so the projector is built");
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
            &mut restored.projector,
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
        match outcome {
            BatchOutcome::Settled { through } => {
                // A batch of nothing but records the offset already covers
                // settles through a position at or below it; the offset never
                // moves back.
                restored.offset = restored.offset.max(through);
                self.report(Settled {
                    id: self.projection_id.clone(),
                    through,
                })
                .await;
                Ok(SettledThrough(through))
            }
            BatchOutcome::Unsettled => Err(ProjectionNotSettled),
        }
    }
}

impl<P, Cs, Tx> Receive<ReadNow> for ProjectorProcess<P, Cs, Tx>
where
    P: Send + Sync + 'static,
    Cs: OffsetStore<Tx = Tx> + Send + Sync + 'static,
    Tx: Send + 'static,
{
    type Response = ();
    type Error = Infallible;

    /// Passed on to the poller, which reads at once.
    async fn recv(
        &mut self,
        msg: ReadNow,
        _ctx: &mut ProcessContext<Self>,
    ) -> Result<(), Infallible> {
        if let Some(poller) = &self.poller {
            // A poller that has given up restarting reads no more; the
            // activator weighs what this projection is owed when it stops.
            if let Err(error) = poller.tell(msg).await {
                tracing::debug!(
                    projection_id = ?self.projection_id,
                    error = %error,
                    "projector: its poller has stopped; the read is not started"
                );
            }
        }
        // Reported whatever the read comes to, so that one that fails does not
        // keep the activator from asking for the next.
        self.report(ReadTakenUp {
            id: self.projection_id.clone(),
        })
        .await;
        Ok(())
    }
}

impl<P, Cs, Tx> Receive<ReachedEnd> for ProjectorProcess<P, Cs, Tx>
where
    P: Send + Sync + 'static,
    Cs: OffsetStore<Tx = Tx> + Send + Sync + 'static,
    Tx: Send + 'static,
{
    type Response = ();
    type Error = Infallible;

    /// Reported only once the offset is known: a read that found nothing from
    /// an unknown offset says nothing of what this activation settled, and
    /// reporting it would have the activator take the activation for one that
    /// needs no second try.
    async fn recv(
        &mut self,
        _msg: ReachedEnd,
        _ctx: &mut ProcessContext<Self>,
    ) -> Result<(), Infallible> {
        match self.restore().await {
            Ok(_offset) => {
                self.report(CaughtUp {
                    id: self.projection_id.clone(),
                })
                .await;
            }
            Err(error) => tracing::warn!(
                projection_id = ?self.projection_id,
                error = %error,
                "projector: loading the recorded offset failed; the end of the journal is not \
                 reported"
            ),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::marker::PhantomData;
    use std::sync::atomic::{AtomicU64, Ordering};

    use async_trait::async_trait;
    use nitinol_persistence::store::{InMemoryEventStore, InMemoryOffsetStore};
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
        let offset_scope = OffsetScope::from(&projection_id);
        let offset_store = InMemoryOffsetStore::default();
        offset_store
            .save(&offset_scope, 5, None)
            .await
            .expect("recording an earlier incarnation's offset must succeed");
        let projected = Arc::new(AtomicU64::new(0));
        let handler: Arc<dyn EventTypeHandler<CountingProjector, ()>> =
            Arc::new(ConcreteHandler::<CountingProjector, Noted> {
                _phantom: PhantomData,
            });
        let system = ProcessSystem::new().await;
        let activator = ProjectionActivator::spawn(&system, None).await;
        let projector = {
            let projected = Arc::clone(&projected);
            let stream = stream.clone();
            Props::new(move || ProjectorProcess {
                projection_id: projection_id.clone(),
                offset_scope: offset_scope.clone(),
                journal: Journal::from(InMemoryEventStore::default()),
                offset_store: offset_store.clone(),
                delivery_mode: DeliveryMode::AtLeastOnce,
                catchup_origin: CatchupOrigin::Aggregate(stream.clone()),
                producer: {
                    let projected = Arc::clone(&projected);
                    Arc::new(move || CountingProjector {
                        projected: Arc::clone(&projected),
                    })
                },
                handlers: vec![Arc::clone(&handler)],
                tx_provider: None,
                poll_interval: crate::DEFAULT_POLL_INTERVAL,
                batch_limit: crate::DEFAULT_BATCH_LIMIT,
                activator: activator.clone(),
                restored: None,
                poller: None,
            })
        };
        let projector = system.spawn(projector).await;

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
