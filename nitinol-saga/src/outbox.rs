//! Outbox-pattern internal events for atomic Persist+Tell.
//!
//! When the interpreter executes a `Persist { events, tells, schedules }`
//! branch, the user events are appended together with one
//! `nitinol.saga.outbox.tell_requested` marker per tell and one
//! `nitinol.saga.outbox.scheduled` marker per schedule, in **one** atomic
//! `EventStore::append` call.  When the dispatch eventually succeeds the
//! retry executor appends a `nitinol.saga.outbox.tell_acked`; if all
//! `RetryPolicy::max_attempts` fail it appends a `nitinol.saga.outbox.tell_failed`.
//!
//! # Reserved prefix
//!
//! The prefix `nitinol.saga.outbox.*` is **reserved** by the framework.
//! User events declared on `Saga::Event` **must not** use this prefix or
//! the replay path will misclassify them.
//!
//! # Payload format
//!
//! `TellAcked` and `TellFailed` markers carry an 8-byte big-endian
//! `tell_id: u64` payload.  `TellRequested` markers carry the same 8-byte
//! `tell_id` header followed by optional **crash-restart bytes** supplied
//! via [`crate::TellIntent::new_with_crash_restart`].  No external codec is
//! involved — the format is defined inside the framework so the replay path
//! can decode the markers without going through `Saga::Event`'s user codec.
//!
//! # Re-dispatch on restart
//!
//! - **Supervised restart** (same OS process): the replay path re-dispatches
//!   via the in-memory [`crate::process::pending_intents::PendingIntents`]
//!   registry.
//! - **Crash restart** (fresh OS process): if the `TellRequested` payload
//!   contains crash-restart bytes **and** the saga was configured with
//!   [`crate::SagaProps::with_crash_restart_factory`], the factory is invoked
//!   with those bytes to reconstruct the [`crate::TellIntent`] and the retry
//!   executor is spawned.  Tells created via [`crate::SagaEffect::tell`]
//!   automatically serialize the command as JSON crash-restart bytes, so a
//!   registered factory can always reconstruct them.  If no factory is
//!   configured, or if the payload carries no crash-restart bytes (i.e. a
//!   `TellRequested` written via [`crate::TellIntent::new`] directly), a
//!   **synthetic `TellFailed`** is appended so the outbox reaches a consistent
//!   terminal state and the Saga can compensate in a future handle.

mod event_types;
mod payload;
mod retry_policy;

pub(crate) use self::payload::{
    decode_tell_id, decode_tell_requested, OutboxAppender, OutboxClassification, OutboxClassifier,
    TerminalKind,
};
pub(crate) use self::retry_policy::RetryPolicy;
