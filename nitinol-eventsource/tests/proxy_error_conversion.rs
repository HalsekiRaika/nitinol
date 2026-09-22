//! What an activation's answer becomes at the reference boundary.
//!
//! `AggregateProcess` answers a command with an `AskHandlerError` and a query
//! with an `ExecHandlerError`, and the runtime wraps either one in its own
//! `AskError` together with the ways a dispatch can fail to arrive at all. A
//! caller of `AggregateProxy` sees [`AskError`] / [`ExecError`], and the
//! translation between the two families is variant-for-variant:
//!
//! - a handler's verdict keeps its meaning, and
//! - every failure of the delivery itself collapses to `Send` — nothing was
//!   decided, because the command never reached an aggregate that could judge
//!   it.
//!
//! The conversion produces this crate's error types, so it belongs to them and
//! is stated as `From`; this file pins it through `From`.
//!
//! `AskError::Persist(Append(SequenceConflict))` is checked with its nesting
//! intact because the proxy reads exactly that shape to decide an activation is
//! on its way out, and `AskError::retryability` reads it to answer whether the
//! command is worth re-issuing. A conversion that flattened it would leave both
//! reading something else.

use nitinol_eventsource::error::{AskHandlerError, ExecHandlerError, PersistError};
use nitinol_eventsource::{AskError, ExecError};
use nitinol_persistence::error::AppendError;
use nitinol_runtime::error::AskError as RuntimeAskError;
use nitinol_runtime::ProcessSystem;

#[derive(Debug, thiserror::Error)]
#[error("the decider refused the command")]
struct Refused;

#[derive(Debug, thiserror::Error)]
#[error("the query could not be answered")]
struct Unanswerable;

/// Given a handler that refused the command, When its answer crosses the
/// boundary, Then the caller sees the refusal — with the decider's own error
/// still inside it.
#[test]
fn a_refusal_stays_a_refusal() {
    let converted: AskError<Refused> =
        RuntimeAskError::Handler(AskHandlerError::Rejection(Refused)).into();

    assert!(
        matches!(converted, AskError::Rejection(Refused)),
        "a decider's refusal must arrive as `Rejection`, got {converted:?}"
    );
}

/// Given a creation that collided with an aggregate that already exists, When
/// its answer crosses the boundary, Then the caller sees that collision, and
/// not a generic persist failure.
#[test]
fn an_already_created_collision_stays_already_created() {
    let converted: AskError<Refused> =
        RuntimeAskError::Handler(AskHandlerError::AlreadyCreated).into();

    assert!(
        matches!(converted, AskError::AlreadyCreated),
        "a collision with an existing aggregate must arrive as `AlreadyCreated`, got {converted:?}"
    );
}

/// Given a persist failure, When it crosses the boundary, Then it arrives whole
/// — the append error it wrapped, and the stream key that error named, are both
/// still readable.
#[test]
fn a_persist_failure_arrives_with_its_own_variant_intact() {
    let converted: AskError<Refused> = RuntimeAskError::Handler(AskHandlerError::Persist(
        PersistError::Append(AppendError::SequenceConflict("order-1".to_owned())),
    ))
    .into();

    assert!(
        matches!(
            &converted,
            AskError::Persist(PersistError::Append(AppendError::SequenceConflict(key)))
                if key == "order-1"
        ),
        "a lost sequence must arrive as `Persist(Append(SequenceConflict))` naming its own \
         stream, got {converted:?}"
    );
}

/// Given a query the aggregate answered with a domain error, When it crosses
/// the boundary, Then the caller sees the domain error.
#[test]
fn a_domain_error_from_a_query_stays_a_domain_error() {
    let converted: ExecError<Unanswerable> =
        RuntimeAskError::Handler(ExecHandlerError::Domain(Unanswerable)).into();

    assert!(
        matches!(converted, ExecError::Domain(Unanswerable)),
        "a query's domain error must arrive as `Domain`, got {converted:?}"
    );
}

/// Given each of the ways a dispatch can fail before any aggregate judged it,
/// When they cross the boundary, Then every one of them arrives as `Send` — on
/// the command surface and on the query surface alike.
///
/// The distinction between them is the runtime's to report; what this layer
/// says is only that nothing was decided, and a conversion that let one of them
/// arrive as a verdict would tell a caller its command was answered when it was
/// never delivered.
#[tokio::test]
async fn every_failure_of_the_delivery_itself_becomes_send() {
    // A `Pid` is minted by the runtime rather than by a caller, so the one the
    // system's own dead-letter stream runs under stands in for a destination.
    let destination = ProcessSystem::new().await.dead_letter_stream().pid();

    let undelivered_commands: [RuntimeAskError<AskHandlerError<Refused>>; 3] = [
        RuntimeAskError::DeadLetter { destination },
        RuntimeAskError::ReplyDropped,
        RuntimeAskError::Timeout { destination },
    ];
    for undelivered in undelivered_commands {
        let described = undelivered.to_string();
        let converted: AskError<Refused> = undelivered.into();

        assert!(
            matches!(converted, AskError::Send(_)),
            "`{described}` means the command reached no aggregate that could judge it, so it \
             must arrive as `Send`, got {converted:?}"
        );
    }

    let undelivered_queries: [RuntimeAskError<ExecHandlerError<Unanswerable>>; 3] = [
        RuntimeAskError::DeadLetter { destination },
        RuntimeAskError::ReplyDropped,
        RuntimeAskError::Timeout { destination },
    ];
    for undelivered in undelivered_queries {
        let described = undelivered.to_string();
        let converted: ExecError<Unanswerable> = undelivered.into();

        assert!(
            matches!(converted, ExecError::Send(_)),
            "`{described}` means the query reached no aggregate that could answer it, so it \
             must arrive as `Send`, got {converted:?}"
        );
    }
}
