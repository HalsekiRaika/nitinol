//! Arm-level visibility for the records the framework writes on a saga stream.
//!
//! The `event_type` column names the declaration and nothing finer, so which
//! arm of a framework record a stored row holds is not legible from the column
//! alone.  Every write of such a record reports the arm here instead, in the
//! place operators already watch.
//!
//! A user's own events are *not* reported: the framework reports what it wrote
//! itself, so a deployment's records do not grow with every domain event a saga
//! persists.  Arm-level measurement of a user's events is the user's to add.

/// Report that a framework record landed on a saga's stream.
///
/// The three fields every such record carries are spelled once, here:
///
/// | field | what it holds |
/// |---|---|
/// | `event_type` | the canonical path the row was written under |
/// | `arm` | which arm of that declaration the record holds, from the family's own fixed set |
/// | `stream_id` | the stream the row landed on |
///
/// Further `name = value` pairs are the calling site's context — a timer, a
/// source, the sequence a consumed position occupies.
///
/// Call this *after* the append succeeds: a record claiming a write that never
/// happened would be worse than none.
macro_rules! report_system_event {
    (
        $event_type:expr, $arm:expr, $stream_id:expr
        $(, $field:ident = $value:expr)* $(,)?
    ) => {
        ::tracing::info!(
            event_type = %$event_type,
            arm = $arm,
            stream_id = $stream_id,
            $($field = $value,)*
            "saga wrote a framework record",
        )
    };
}

pub(crate) use report_system_event;
