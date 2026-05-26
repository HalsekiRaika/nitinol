use std::borrow::Borrow;
use std::sync::Arc;

use futures_util::StreamExt;
use nitinol_eventsource::codec::ErasedCodec;
use nitinol_eventsource::EventEnvelope;
use nitinol_persistence::store::EventStore;
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
    pub(crate) store: Arc<dyn EventStore>,
    pub(crate) codec: Arc<dyn ErasedCodec<S::Event>>,
    pub(crate) route_fn: RouteFn<S::SubscribedEvent>,
    pub(crate) sequence: u64,
}

impl<S: Saga> Process for SagaProcess<S> {
    async fn on_start(&mut self, _ctx: &mut ProcessContext) {
        let query = LoadQuery {
            stream_key: Some(self.saga_id.as_str().to_owned()),
            from_stream_sequence: Some(self.sequence + 1),
            ..Default::default()
        };

        let stream = match self.store.load(query).await {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = ?e, "saga event store load failed during replay");
                return;
            }
        };

        futures_util::pin_mut!(stream);
        while let Some(item) = stream.next().await {
            let loaded = match item {
                Ok(ev) => ev,
                Err(e) => {
                    tracing::error!(error = ?e, "saga event store stream error during replay");
                    return;
                }
            };
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
            self.store.as_ref(),
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
    store: &'a dyn EventStore,
    codec: &'a dyn ErasedCodec<S::Event>,
) -> futures_core::future::BoxFuture<'a, ()> {
    Box::pin(async move {
        match effect {
            SagaEffect::None => {}

            SagaEffect::Persist(events) => {
                persist_events(events, state, saga_id, sequence, store, codec).await;
            }

            SagaEffect::Tell(SagaTellEffect(side)) => {
                dispatch_tell(side);
            }

            SagaEffect::Sequence(effects) => {
                for sub in effects {
                    run_saga_effect(sub, state, saga_id, sequence, store, codec).await;
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
    store: &dyn EventStore,
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
            sequence: next_sequence,
            event_type: <S::Event as nitinol_eventsource::Event>::EVENT_TYPE,
            payload,
            occurred_at: jiff::Timestamp::now(),
        });
    }

    if let Err(e) = store.append(saga_id.borrow(), appending).await {
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
