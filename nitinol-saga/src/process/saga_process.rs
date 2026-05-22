use std::sync::Arc;

use nitinol_eventsource::codec::ErasedCodec;
use nitinol_eventsource::EventEnvelope;
use nitinol_eventsource::EventPersistorProxy;
use nitinol_persistence::{AppendingEvent, LoadQuery};
use nitinol_runtime::process::{Process, ProcessContext, Receive};

use crate::context::SagaContext;
use crate::effect::{SagaEffect, SagaSideEffect, SagaTellEffect};
use crate::id::SagaId;
use crate::saga::Saga;

pub(crate) type RouteFn<E> = Arc<dyn Fn(&E) -> Option<SagaId> + Send + Sync>;

pub struct SagaProcess<S: Saga> {
    pub(crate) state: S,
    pub(crate) saga_id: SagaId,
    pub(crate) event_ref: EventPersistorProxy,
    pub(crate) codec: Arc<dyn ErasedCodec<S::Event>>,
    pub(crate) route_fn: RouteFn<S::SubscribedEvent>,
    pub(crate) sequence: u64,
}

impl<S: Saga> Process for SagaProcess<S> {
    async fn on_start(&mut self, _ctx: &mut ProcessContext) {
        let query = LoadQuery {
            aggregate_id: Some(self.saga_id.clone()),
            from_aggregate_sequence: Some(self.sequence + 1),
            ..Default::default()
        };

        let events = match self.event_ref.load(query).await {
            Ok(evts) => evts,
            Err(e) => {
                tracing::error!(error = ?e, "saga event store load failed during replay");
                return;
            }
        };

        for loaded in events {
            match self.codec.decode(&loaded.payload) {
                Ok(event) => {
                    self.state.apply(event);
                    self.sequence = loaded.sequence;
                }
                Err(e) => {
                    tracing::error!(error = %e, "saga event decode failed; skipping event");
                }
            }
        }
    }
}

impl<S: Saga> Receive<EventEnvelope<S::SubscribedEvent>> for SagaProcess<S> {
    type Response = ();
    type Error = std::convert::Infallible;

    async fn recv(
        &mut self,
        msg: EventEnvelope<S::SubscribedEvent>,
        _ctx: &mut ProcessContext,
    ) -> Result<(), std::convert::Infallible> {
        let Some(target_id) = (self.route_fn)(&msg.event) else {
            return Ok(());
        };
        if target_id != self.saga_id {
            return Ok(());
        }

        let mut saga_ctx = SagaContext::new(self.saga_id.clone(), self.sequence);
        let effect = match self.state.handle(msg.event, &mut saga_ctx).await {
            Ok(effect) => effect,
            Err(e) => {
                tracing::warn!(error = %e, "saga handle failed");
                return Ok(());
            }
        };

        run_saga_effect(
            effect,
            &mut self.state,
            &self.saga_id,
            &mut self.sequence,
            &self.event_ref,
            self.codec.as_ref(),
        )
        .await;
        Ok(())
    }
}

fn run_saga_effect<'a, S: Saga>(
    effect: SagaEffect<S::Event>,
    state: &'a mut S,
    saga_id: &'a SagaId,
    sequence: &'a mut u64,
    event_ref: &'a EventPersistorProxy,
    codec: &'a dyn ErasedCodec<S::Event>,
) -> futures_core::future::BoxFuture<'a, ()> {
    Box::pin(async move {
        match effect {
            SagaEffect::None => {}

            SagaEffect::Persist(events) => {
                persist_events(events, state, saga_id, sequence, event_ref, codec).await;
            }

            SagaEffect::Tell(SagaTellEffect(side)) => {
                dispatch_tell(side);
            }

            SagaEffect::Sequence(effects) => {
                for sub in effects {
                    run_saga_effect(sub, state, saga_id, sequence, event_ref, codec).await;
                }
            }
        }
    })
}

async fn persist_events<S: Saga>(
    events: Vec<S::Event>,
    state: &mut S,
    saga_id: &SagaId,
    sequence: &mut u64,
    event_ref: &EventPersistorProxy,
    codec: &dyn ErasedCodec<S::Event>,
) {
    let mut next_sequence = *sequence;
    let mut appending = Vec::with_capacity(events.len());
    for event in &events {
        next_sequence += 1;
        let payload = match codec.encode(event) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, "saga event encode failed; skipping persist");
                return;
            }
        };
        appending.push(AppendingEvent {
            aggregate_id: saga_id.clone(),
            sequence: next_sequence,
            event_type: <S::Event as nitinol_eventsource::Event>::EVENT_TYPE,
            payload,
            occurred_at: jiff::Timestamp::now(),
        });
    }

    if let Err(e) = event_ref.append(saga_id.clone(), appending).await {
        tracing::warn!(error = %e, "saga event append failed; skipping apply");
        return;
    }

    *sequence = next_sequence;
    for event in events {
        state.apply(event);
    }
}

fn dispatch_tell(side: Box<dyn SagaSideEffect>) {
    tokio::spawn(async move {
        if let Err(e) = side.execute().await {
            tracing::warn!(error = %e, "saga side effect failed");
        }
    });
}
