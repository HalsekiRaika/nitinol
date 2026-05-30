//! Shared registry of in-flight [`TellIntent`]s.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::effect::TellIntent;

/// A shared registry of in-flight [`TellIntent`]s keyed by `tell_id`.
///
/// The interpreter inserts an entry when it appends a `TellRequested` batch;
/// the outbox executor removes it when it appends the terminal marker
/// (`TellAcked` or `TellFailed`).
///
/// On supervised restart (same OS-process lifetime) the registry still holds
/// any TellIntent whose executor did not yet complete.  `replay_and_redispatch`
/// checks this registry: a matching entry is re-dispatched to the retry
/// executor (true re-dispatch per spec C-9); a missing entry means a
/// crash-restart where re-dispatch is not possible in this MVP.
///
/// Created automatically by [`crate::SagaProps::spawn`].  The builder method
/// [`crate::SagaProps::with_pending_intents`] allows sharing the registry
/// across multiple spawns of the same saga in crate-internal tests of
/// supervised restart behaviour.
#[derive(Clone)]
pub(crate) struct PendingIntents(pub(crate) Arc<Mutex<HashMap<u64, TellIntent>>>);

impl Default for PendingIntents {
    fn default() -> Self {
        Self(Arc::new(Mutex::new(HashMap::new())))
    }
}

impl PendingIntents {
    /// Create an empty registry.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Register a pending [`TellIntent`] for `tell_id`.
    ///
    /// Called by the saga interpreter before spawning each outbox executor,
    /// and by crate-internal tests to pre-populate the registry for simulating
    /// a supervised restart.
    pub(crate) async fn register(&self, tell_id: u64, intent: TellIntent) {
        self.0.lock().await.insert(tell_id, intent);
    }

    /// Return a clone of the [`TellIntent`] for `tell_id` without removing it.
    ///
    /// Used by `replay_and_redispatch` to obtain the intent for supervised-restart
    /// re-dispatch: the entry stays in the registry so that the spawned retry
    /// executor can remove it only after the terminal marker is durably appended.
    /// This preserves the invariant that every pending tell remains in the registry
    /// until the outbox is settled.
    pub(crate) async fn get(&self, tell_id: u64) -> Option<TellIntent> {
        self.0.lock().await.get(&tell_id).cloned()
    }

    /// Remove and return the [`TellIntent`] for `tell_id`, if present.
    pub(crate) async fn take(&self, tell_id: u64) -> Option<TellIntent> {
        self.0.lock().await.remove(&tell_id)
    }
}
