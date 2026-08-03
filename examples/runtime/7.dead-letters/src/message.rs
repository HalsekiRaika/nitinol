//! Message types for the dead-letters example.
//!
//! Three message types demonstrate the different DLQ scenarios:
//! - `Ping`: a plain tell message — shows basic DLQ routing
//! - `Query`: an ask message — shows `AskError::DeadLetter` and stream delivery
//! - `Hush`: a tell message with `SuppressDeadLetterLog` — shows that log suppression
//!   does NOT prevent stream delivery; the dead-letter stream still receives the notification

use nitinol_runtime::SuppressDeadLetterLog;

// ─── Messages ─────────────────────────────────────────────────────────────────

/// A plain tell message used to demonstrate basic DLQ routing.
pub struct Ping;

/// An ask message used to demonstrate `AskError::DeadLetter` and stream delivery.
///
/// `TargetProcess` responds with `u32`.
pub struct Query;

/// A tell message that implements `SuppressDeadLetterLog`.
///
/// The runtime suppresses the log entry for this type, but the dead-letter
/// stream still receives the notification — suppression is log-only.
pub struct Hush;

impl SuppressDeadLetterLog for Hush {}
