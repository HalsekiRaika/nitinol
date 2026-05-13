use std::sync::Arc;

use futures_util::StreamExt;
use nitinol_persistence::error::{AppendError, LoadError};
use nitinol_persistence::store::EventStore;
use nitinol_persistence::{AggregateId, AppendingEvent, LoadedEvent, LoadQuery};
use nitinol_runtime::process::{Process, ProcessContext, Receive};
use nitinol_runtime::process::ProcessProxy;
use nitinol_runtime::{IdleTimeout, Props, ProcessSystem, SupervisionStrategy};

// ---------------------------------------------------------------------------
// Internal messages
//
// `pub(crate)` so AggregateProcess can construct them directly for
// fine-grained error mapping in run_effect.  External callers use the
// high-level methods on EventPersistorRef instead.
// ---------------------------------------------------------------------------

pub(crate) struct AppendEvents {
    pub(crate) aggregate_id: AggregateId,
    pub(crate) events: Vec<AppendingEvent>,
}

pub(crate) struct LoadEvents {
    pub(crate) query: LoadQuery,
}

// ---------------------------------------------------------------------------
// EventPersistor
// ---------------------------------------------------------------------------

/// Persistence actor that exclusively owns an `Arc<dyn EventStore>`.
///
/// All event store operations are serialised through this actor's message loop.
/// No other process holds a reference to the store — satisfying the Phase 2
/// lockless, DB-lock-free design.
pub struct EventPersistor {
    store: Arc<dyn EventStore>,
}

impl Process for EventPersistor {}

impl Receive<AppendEvents> for EventPersistor {
    type Response = ();
    type Error = AppendError;

    async fn recv(
        &mut self,
        msg: AppendEvents,
        _ctx: &mut ProcessContext,
    ) -> Result<(), AppendError> {
        self.store
            .append(&msg.aggregate_id, msg.events)
            .await
            .map(|_| ())
    }
}

impl Receive<LoadEvents> for EventPersistor {
    type Response = Vec<LoadedEvent>;
    type Error = LoadError;

    async fn recv(
        &mut self,
        msg: LoadEvents,
        _ctx: &mut ProcessContext,
    ) -> Result<Vec<LoadedEvent>, LoadError> {
        let stream = self.store.load(msg.query).await?;
        futures_util::pin_mut!(stream);
        let mut results = Vec::new();
        while let Some(item) = stream.next().await {
            results.push(item?);
        }
        Ok(results)
    }
}

impl EventPersistor {
    /// Spawn an `EventPersistor` on the given system and return a typed reference.
    ///
    /// - Supervision: `Resume` — actor stays alive after handler errors.
    /// - Idle timeout: `Persistent` — never stopped due to inactivity.
    pub async fn spawn(system: &ProcessSystem, store: Arc<dyn EventStore>) -> EventPersistorProxy {
        let mut props = Props::new(move || EventPersistor {
            store: Arc::clone(&store),
        });
        props.with_supervision_strategy(SupervisionStrategy::Resume);
        props.with_idle_timeout(IdleTimeout::Persistent);
        EventPersistorProxy(system.spawn(props).await)
    }
}

// ---------------------------------------------------------------------------
// EventPersistorRef
// ---------------------------------------------------------------------------

/// Typed, cloneable handle to an `EventPersistor` actor.
///
/// Callers append and load events without holding any direct store reference.
/// The inner proxy is `pub(crate)` so `run_effect` can perform a raw `ask` for
/// fine-grained error mapping between handler errors and connectivity errors.
#[derive(Clone)]
pub struct EventPersistorProxy(pub(crate) ProcessProxy<EventPersistor>);

impl EventPersistorProxy {
    /// Append events to the store via the `EventPersistor` actor.
    pub async fn append(
        &self,
        aggregate_id: AggregateId,
        events: Vec<AppendingEvent>,
    ) -> Result<(), AppendError> {
        self.0
            .ask(AppendEvents { aggregate_id, events })
            .await
            .map_err(|e| match e {
                nitinol_runtime::error::AskError::Handler(e) => e,
                _ => AppendError::Backend(Box::new(nitinol_runtime::error::SendError)),
            })
    }

    /// Load events from the store via the `EventPersistor` actor.
    pub async fn load(&self, query: LoadQuery) -> Result<Vec<LoadedEvent>, LoadError> {
        self.0
            .ask(LoadEvents { query })
            .await
            .map_err(|e| match e {
                nitinol_runtime::error::AskError::Handler(e) => e,
                _ => LoadError::Backend(Box::new(nitinol_runtime::error::SendError)),
            })
    }
}
