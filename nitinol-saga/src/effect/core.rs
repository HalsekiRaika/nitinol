use std::marker::PhantomData;
use std::sync::Arc;

use bytes::Bytes;
use futures_core::future::BoxFuture;
use nitinol_eventsource::{Aggregate, AggregateTellTarget, Decider};

use crate::effect::tell::TypedSagaTell;
use crate::error::SagaSideEffectError;

/// A declarative description of effects produced by a [`crate::Saga::handle`]
/// call.
///
/// `SagaEffect<E>` is a Monoid: [`SagaEffect::None`] is the identity element
/// and [`SagaEffect::combine`] is the associative binary operation.  The
/// [`SagaEffect::Sequence`] variant accumulates leaves in a flat list;
/// `combine` introduces no nesting.
///
/// # Variants
///
/// - `None` — Monoid identity, no-op.
/// - `Persist { events, tells, schedules }` — append the user events together
///   with one `TellRequested` outbox marker per tell, and one `Scheduled`
///   marker per schedule, **in a single atomic batch** on the saga's own
///   event store.  After the append succeeds, each tell is dispatched via the
///   retry executor.  Schedules are reserved (interpreter is a no-op in this
///   MVP).
/// - `End` — single-responsibility termination marker.  Stops the saga
///   process and tears down its upstream subscription.  Effects placed after
///   `End` inside a `Sequence` are not interpreted.
/// - `Sequence(Vec<SagaEffect<E>>)` — Monoid composition.  Interpreted left
///   to right with short-circuit on `End`.
pub enum SagaEffect<E> {
    /// No-op — the identity element of the Monoid.
    None,
    /// Persist a batch of user events together with the per-tell `TellRequested`
    /// markers and the per-schedule `Scheduled` markers in one atomic
    /// `store::append` call.  See type-level docs for the full contract.
    Persist {
        events: Vec<E>,
        tells: Vec<TellIntent>,
        schedules: Vec<Schedule>,
    },
    /// Stop the saga process.  Released here so the runtime can drop the
    /// upstream `DurableStream` keepalive and prevent further `handle` calls.
    End,
    /// An ordered collection of effects executed sequentially.
    Sequence(Vec<SagaEffect<E>>),
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
/// This enables [`crate::process::pending_intents::PendingIntents`] to store a
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
    /// Opaque bytes appended after the 8-byte `tell_id` in the `TellRequested`
    /// outbox marker.  The saga's crash-restart factory uses these bytes to
    /// reconstruct the `TellIntent` after a full OS-process crash.
    ///
    /// `None` means crash-restart re-dispatch is not supported for this intent
    /// (supervised restart via [`crate::process::pending_intents::PendingIntents`]
    /// still works).
    pub(crate) crash_restart_payload: Option<Bytes>,
}

impl Clone for TellIntent {
    fn clone(&self) -> Self {
        Self {
            side: Arc::clone(&self.side),
            crash_restart_payload: self.crash_restart_payload.clone(),
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
        Self {
            side: Arc::new(TypedSagaTell {
                target,
                cmd,
                _phantom: PhantomData::<fn() -> A>,
            }),
            crash_restart_payload: None,
        }
    }

    /// Build a `TellIntent` with an optional crash-restart payload.
    ///
    /// The `crash_restart_payload` bytes are stored in the `TellRequested`
    /// outbox marker (after the 8-byte `tell_id`) so that a
    /// [`crate::SagaProps::with_crash_restart_factory`]-equipped saga can
    /// reconstruct the intent after a full OS-process crash.
    ///
    /// The factory receives exactly the bytes supplied here and must return a
    /// `TellIntent` that re-sends the same command to the same target.
    pub fn new_with_crash_restart<A, C, T>(
        target: T,
        cmd: C,
        crash_restart_payload: Bytes,
    ) -> Self
    where
        A: Aggregate + Decider<C>,
        C: Clone + Send + Sync + 'static,
        T: AggregateTellTarget<A>,
    {
        Self {
            side: Arc::new(TypedSagaTell {
                target,
                cmd,
                _phantom: PhantomData::<fn() -> A>,
            }),
            crash_restart_payload: Some(crash_restart_payload),
        }
    }
}

/// A timer registration carried inside a `Persist` branch.
///
/// In this MVP the interpreter only appends a `Scheduled` outbox marker as
/// part of the same atomic batch as the user events.  Actual timer
/// execution is reserved for a follow-up issue (γ).  The `at` field is
/// `pub` so test code and future scheduler implementations can both read
/// the wall-clock target without going through an accessor.
#[derive(Debug, Clone)]
pub struct Schedule {
    pub at: jiff::Timestamp,
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
