mod message;
mod payload;
mod retry_policy;

pub(crate) use self::message::{is_outbox_event_type, OutboxEvent};
// The marker payload structs are an internal detail of `OutboxEvent`'s codec in
// production, but tests need them to build fold inputs without a store.
#[cfg(test)]
pub(crate) use self::message::{Ended, Scheduled, TellAcked, TellFailed, TellRequested};
pub(crate) use self::payload::{OutboxAppender, TellOutcome};
pub(crate) use self::retry_policy::RetryPolicy;
