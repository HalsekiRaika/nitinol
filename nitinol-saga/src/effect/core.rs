use std::marker::PhantomData;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures_core::future::BoxFuture;
use nitinol_eventsource::{Aggregate, AggregateTellTarget, Decider};

use crate::effect::tell::TypedSagaTell;
use crate::error::SagaSideEffectError;
use crate::id::SagaId;
use crate::scheduler::TimerName;

/// A declarative description of effects produced by a [`crate::Saga::handle`]
/// call.
///
/// `SagaEffect<E>` is a Monoid: [`SagaEffect::None`] is the identity element
/// and [`SagaEffect::combine`] is the associative binary operation.  The
/// [`SagaEffect::Sequence`] variant accumulates leaves in a flat list;
/// `combine` introduces no nesting and folds an adjacent `Persist × Persist`
/// junction into one `Persist` so the pair is written as a single atomic batch.
///
/// # Variants
///
/// - `None` — Monoid identity, no-op.
/// - `Persist { events, tells, schedules }` — append the user events together
///   with one `TellRequested` outbox marker per tell, and one
///   `ScheduleEvent::Scheduled` marker per schedule, **in a single atomic
///   batch** on the saga's own event store.  After the append succeeds, each
///   tell is dispatched via the retry executor and each schedule is registered
///   with the resident `SchedulerProcess` via the injected [`SchedulerProxy`].
///   A branch whose `events`, `tells` and `schedules` are all empty carries
///   nothing to append and is interpreted exactly like `None`.
/// - `End` — single-responsibility termination marker.  Stops the saga
///   process and tears down its upstream subscription.  Effects placed after
///   `End` inside a `Sequence` are not interpreted.
/// - `Sequence(Vec<SagaEffect<E>>)` — Monoid composition.  Interpreted left
///   to right with short-circuit on `End`.  All variants are public, so a
///   `Sequence` can be hand-built without going through `combine`; the
///   interpreter re-applies `combine`'s adjacent `Persist × Persist` merge to
///   every effect before interpreting it, so the single-atomic-batch
///   guarantee documented on `Persist` holds regardless of how the effect was
///   constructed.
/// - `CancelSchedule(TimerName)` — name-scoped timer cancellation.  Persists a
///   `ScheduleEvent::Cancelled` marker and cancels the pending timer.
///
/// [`SchedulerProxy`]: crate::SchedulerProxy
pub enum SagaEffect<E> {
    /// No-op — the identity element of the Monoid.
    None,
    /// Persist a batch of user events together with the per-tell `TellRequested`
    /// markers and the per-schedule `ScheduleEvent::Scheduled` markers in one
    /// atomic `store::append` call.  See type-level docs for the full contract.
    Persist {
        events: Vec<E>,
        tells: Vec<TellIntent>,
        schedules: Vec<ScheduleSpec>,
    },
    /// Stop the saga process.  The runtime cascade-stops the upstream
    /// `DirectPollerProcess` child, preventing further `handle` calls.
    End,
    /// An ordered collection of effects executed sequentially.
    Sequence(Vec<SagaEffect<E>>),
    /// Cancel the timer registered under a [`TimerName`] for this saga.
    /// Interpreted as a `ScheduleEvent::Cancelled` append plus a
    /// scheduler cancel; equivalent to token-scoped cancel since the saga id is
    /// fixed to the emitting saga.
    CancelSchedule(TimerName),
}

/// Opaque description of a typed command-to-aggregate dispatch.
///
/// Constructed via [`TellIntent::new`] (no crash-restart support) or
/// [`TellIntent::new_with_crash_restart`] (explicit crash-restart payload).
/// [`crate::SagaEffect::tell`] automatically serializes the command via
/// `serde_json` and uses [`TellIntent::new_with_crash_restart`] so that crash-
/// restart re-dispatch is available for all tells built through that API.
///
/// `Clone` is required because the retry executor re-`tell`s with a cloned
/// command on every attempt.
///
/// `TellIntent` is `Clone` (cheap — the inner `Arc` is reference-counted).
/// This enables the in-memory `tell_states` registry to store a
/// clone before transferring ownership to the outbox executor.
///
/// # Crash-restart re-dispatch
///
/// `TellIntent::new` does **not** support crash-restart re-dispatch —
/// the in-memory `Arc<dyn SagaSideEffect>` is lost when the OS process dies.
/// Use [`TellIntent::new_with_crash_restart`] (or [`crate::SagaEffect::tell`]
/// which does this automatically) together with
/// [`crate::SagaProps::with_crash_restart_factory`] to opt in: the supplied
/// `crash_restart_payload` bytes are appended to the `TellRequested` outbox
/// marker so the factory can reconstruct the intent after a full process
/// restart.
pub struct TellIntent {
    pub(crate) side: Arc<dyn SagaSideEffect>,
    /// Opaque bytes stored in prost field 2 (`crash_restart`) of the
    /// `TellRequested` outbox marker.  The saga's crash-restart factory uses
    /// these bytes to reconstruct the `TellIntent` after a full OS-process
    /// crash.
    ///
    /// `None` means crash-restart re-dispatch is not supported for this intent
    /// (supervised restart via the in-memory `tell_states` registry
    /// still works).  An empty slice is normalised to `None` at the write path.
    pub(crate) crash_restart_payload: Option<Bytes>,
    /// The target aggregate's stream key, captured from
    /// [`AggregateTellTarget::aggregate_id_str`] at construction time.  Stored
    /// so the saga can write `SagaFailure::TellFailed::target` when the tell
    /// exhausts its retry budget.
    pub(crate) target_id: SagaId,
}

impl Clone for TellIntent {
    fn clone(&self) -> Self {
        Self {
            side: Arc::clone(&self.side),
            crash_restart_payload: self.crash_restart_payload.clone(),
            target_id: self.target_id.clone(),
        }
    }
}

impl TellIntent {
    /// Build a `TellIntent` over a typed aggregate target.
    ///
    /// `C: Clone` is required because the retry executor keeps the command in
    /// memory across attempts and re-`tell`s with a cloned copy on every
    /// retry.
    ///
    /// This constructor does **not** support crash-restart re-dispatch.  Use
    /// [`TellIntent::new_with_crash_restart`] when you need re-dispatch after
    /// a full OS-process crash.
    pub fn new<A, C, T>(target: T, cmd: C) -> Self
    where
        A: Aggregate + Decider<C>,
        C: Clone + Send + Sync + 'static,
        T: AggregateTellTarget<A>,
    {
        let target_id_str = target.aggregate_id_str();
        assert!(
            !target_id_str.is_empty(),
            "TellIntent: target returned an empty aggregate_id_str(); \
             implement AggregateTellTarget::aggregate_id_str() to return \
             the target aggregate's stream key for TellFailed DLQ tracking"
        );
        let target_id = SagaId::new(target_id_str);
        Self {
            side: Arc::new(TypedSagaTell {
                target,
                cmd,
                _phantom: PhantomData::<fn() -> A>,
            }),
            crash_restart_payload: None,
            target_id,
        }
    }

    /// Build a `TellIntent` with an optional crash-restart payload.
    ///
    /// The `crash_restart_payload` bytes are stored in prost field 2
    /// (`crash_restart`) of the `TellRequested` outbox marker so that a
    /// [`crate::SagaProps::with_crash_restart_factory`]-equipped saga can
    /// reconstruct the intent after a full OS-process crash.
    ///
    /// The factory receives exactly the bytes supplied here and must return a
    /// `TellIntent` that re-sends the same command to the same target.
    pub fn new_with_crash_restart<A, C, T>(target: T, cmd: C, crash_restart_payload: Bytes) -> Self
    where
        A: Aggregate + Decider<C>,
        C: Clone + Send + Sync + 'static,
        T: AggregateTellTarget<A>,
    {
        let target_id_str = target.aggregate_id_str();
        assert!(
            !target_id_str.is_empty(),
            "TellIntent: target returned an empty aggregate_id_str(); \
             implement AggregateTellTarget::aggregate_id_str() to return \
             the target aggregate's stream key for TellFailed DLQ tracking"
        );
        let target_id = SagaId::new(target_id_str);
        Self {
            side: Arc::new(TypedSagaTell {
                target,
                cmd,
                _phantom: PhantomData::<fn() -> A>,
            }),
            crash_restart_payload: Some(crash_restart_payload),
            target_id,
        }
    }
}

/// A timer registration carried inside a `Persist` branch.
///
/// The interpreter appends a `ScheduleEvent::Scheduled` marker as part of the
/// same atomic batch as the user events, then registers the timer with the
/// resident scheduler.  The delay is relative (`after: Duration`), not an
/// absolute instant, so it survives replay via a persisted wall-clock anchor.
/// `payload` is the serialized `Saga::ScheduledMessage`.  The fields are `pub`
/// so a saga can build a spec directly and put it on an effect through
/// [`crate::SagaEffect::schedule_spec`], or let
/// [`crate::SagaEffect::schedule`] build both from a typed message.
#[derive(Debug, Clone, PartialEq)]
pub struct ScheduleSpec {
    pub name: TimerName,
    pub after: Duration,
    pub payload: Bytes,
}

/// A type-safe, object-safe side effect that can be executed asynchronously
/// from within the saga effect interpreter.
///
/// Constructed internally by [`TellIntent::new`] / [`SagaEffect::tell`]; not
/// part of the public API.  `execute_once(&self)` is invoked per retry
/// attempt; the trait deliberately consumes only `&self` so the executor can
/// retain ownership across retries.
pub(crate) trait SagaSideEffect: Send + Sync + 'static {
    fn execute_once(&self) -> BoxFuture<'_, Result<(), SagaSideEffectError>>;
}
