//! The mapping from a decision to its deliveries is a pure associated
//! function.
//!
//! ```gherkin
//!   Scenario: the mapping needs no runtime
//!     Given a saga type that declares a mapping
//!     When the mapping is applied to a decision event with nothing running
//!     Then it answers the target and the command the decision implies
//!
//!   Scenario: a saga that declares no mapping
//!     Given a saga type that leaves the mapping to the trait's default
//!     Then it delivers nothing
//! ```
//!
//! Every test in this file is a plain `#[test]`: no process system, no store,
//! no `.await`. That is the whole point of the contract — a mapping taking a
//! receiver, a system handle or an `async` signature could not be called from
//! here at all, and the delivery knowledge (polling, cursor, checkpoint,
//! stalling) stays with the framework rather than reaching the user's mapping.

#[path = "common/delivery.rs"]
mod delivery;
use delivery::{Decided, DeliverySaga, PartlyRefusedSaga, SilentSaga, REFUSED_SUFFIX};

use nitinol_saga::Saga;

fn decision(target: &str, operation: &str) -> Decided {
    Decided {
        target: target.to_owned(),
        operation: operation.to_owned(),
    }
}

/// The targets a mapping answers, as stream keys, in declared order.
fn targets_of<S: Saga>(event: &S::Event) -> Vec<String> {
    S::dispatch(event)
        .iter()
        .map(|delivery| delivery.target().as_str().to_owned())
        .collect()
}

#[test]
fn a_declared_mapping_answers_one_delivery_per_consequence() {
    let event = decision("ledger-1", "op-1");

    assert_eq!(
        targets_of::<DeliverySaga>(&event),
        vec!["ledger-1".to_owned()],
        "a decision with one consequence maps to one delivery, addressed to \
         the stream key the decision itself carries"
    );
}

#[test]
fn a_mapping_may_answer_several_deliveries_in_declared_order() {
    let event = decision("ledger-2", "op-2");

    assert_eq!(
        targets_of::<PartlyRefusedSaga>(&event),
        vec!["ledger-2".to_owned(), format!("ledger-2{REFUSED_SUFFIX}"),],
        "one decision whose consequences live on two streams maps to two \
         deliveries, in the order the mapping declared them"
    );
}

#[test]
fn a_saga_that_declares_no_mapping_delivers_nothing() {
    let event = decision("ledger-3", "op-3");

    assert!(
        targets_of::<SilentSaga>(&event).is_empty(),
        "the mapping is defaulted, so a saga written before it existed keeps \
         compiling and delivers nothing"
    );
}

#[test]
fn the_mapping_is_a_function_of_the_decision_alone() {
    let event = decision("ledger-4", "op-4");

    assert_eq!(
        targets_of::<DeliverySaga>(&event),
        targets_of::<DeliverySaga>(&event),
        "applying the mapping twice to one decision answers the same \
         deliveries: it reads nothing but the event it was handed"
    );

    assert_eq!(
        targets_of::<DeliverySaga>(&decision("ledger-5", "op-5")),
        vec!["ledger-5".to_owned()],
        "a different decision answers a different target, so the mapping is \
         reading the event rather than a constant"
    );
}
