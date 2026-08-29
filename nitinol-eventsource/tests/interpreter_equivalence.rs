//! One decider, two interpreters, one set of observations.
//!
//! ```gherkin
//! Scenario: the observational equivalence of two interpreters
//!   Given one Decider implementation
//!   When it is consumed both through an aggregate activation and through an
//!        external executor that never touches the process runtime
//!   Then what the domain observes is the same on both paths
//! ```
//!
//! The two interpreters share nothing but the decider: one dispatches through
//! `AggregateProxy` onto a runtime process, the other replays, decides, appends
//! and applies inline.  Neither is written against the other.
//!
//! The external executor is put through the conformance suite first.  Comparing
//! two interpreters says nothing on its own — two identically wrong ones agree
//! perfectly — so each side has to be independently correct before their
//! answers are worth comparing at all.
//!
//! What "the domain observes" is taken to be everything the domain can reach:
//! the verdict of every command, the answer to every question, and the facts
//! the stream ends up holding under the sequence numbers they were given.
//!
//! The subscriber that captures a surfaced refusal is installed globally, which
//! can happen once per process, so this file holds exactly one test.

#[path = "common/interpreters.rs"]
mod common;
use common::{
    ActivationInterpretation, ActivationInterpreter, DirectInterpretation, DirectInterpreter,
    ReportCapture,
};

use std::fmt::Debug;
use std::sync::Arc;

use futures_util::TryStreamExt;

use nitinol_conformance::{
    verify, Balance, Holder, Interpretation, Interpreted, Interpreter, Ledger, LedgerEvent,
    LedgerNotOpen, LedgerRejection, Open, Settle, Unanswered,
};
use nitinol_eventsource::{Decider, Query};
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::{AggregateId, LoadQuery};
use nitinol_runtime::ProcessSystem;

/// The holder the ledger is opened for — a domain fact, unrelated to either
/// stream key, so a `Holder` answer taken from the stream key rather than from
/// the creation event would not match it.
const HOLDER: &str = "acct-7";

// Putting the same question to both interpreters

/// Issue one command on both paths and hand back the two verdicts.
///
/// The command is built twice rather than cloned so that neither interpreter is
/// handed a value the other has already seen.
async fn ask_both<C>(
    activation: &ActivationInterpreter,
    direct: &DirectInterpreter,
    command: fn() -> C,
) -> (
    Interpreted<<Ledger as Decider<C>>::Output>,
    Interpreted<<Ledger as Decider<C>>::Output>,
)
where
    Ledger: Decider<C, Rejection = LedgerRejection>,
    C: Send + Sync + 'static,
    <Ledger as Decider<C>>::Output: Send + 'static,
{
    let by_activation = activation.ask(command()).await;
    let by_direct = direct.ask(command()).await;
    (by_activation, by_direct)
}

/// Ask one question on both paths and hand back the two answers.
async fn exec_both<M>(
    activation: &ActivationInterpreter,
    direct: &DirectInterpreter,
    question: fn() -> M,
) -> (
    Result<<Ledger as Query<M>>::Response, Unanswered>,
    Result<<Ledger as Query<M>>::Response, Unanswered>,
)
where
    Ledger: Query<M, Error = LedgerNotOpen>,
    M: Send + Sync + 'static,
    <Ledger as Query<M>>::Response: Send + 'static,
{
    let by_activation = activation.exec(question()).await;
    let by_direct = direct.exec(question()).await;
    (by_activation, by_direct)
}

// Comparing what the domain saw

/// Both paths must have reached the same verdict on one command.
///
/// A refusal is compared by what it says, because that is all the domain is
/// handed. A `Failed` on either side is never an equivalence: it says the
/// machinery got in the way, so there is no verdict to compare.
fn assert_same_verdict<O>(activation: &Interpreted<O>, direct: &Interpreted<O>, command: &str)
where
    O: Debug + PartialEq,
{
    match (activation, direct) {
        (Interpreted::Answered(by_activation), Interpreted::Answered(by_direct)) => assert_eq!(
            by_activation, by_direct,
            "{command}: the two interpreters answered the domain differently"
        ),
        (Interpreted::Refused(by_activation), Interpreted::Refused(by_direct)) => assert_eq!(
            by_activation.to_string(),
            by_direct.to_string(),
            "{command}: the two interpreters carried back different refusals"
        ),
        (Interpreted::AlreadyCreated, Interpreted::AlreadyCreated) => {}
        _ => panic!(
            "{command}: the two interpreters reached different verdicts — \
             activation {activation:?}, external executor {direct:?}"
        ),
    }
}

/// Both paths must have given the same answer to one question.
fn assert_same_answer<R>(
    activation: &Result<R, Unanswered>,
    direct: &Result<R, Unanswered>,
    question: &str,
) where
    R: Debug + PartialEq,
{
    match (activation, direct) {
        (Ok(by_activation), Ok(by_direct)) => assert_eq!(
            by_activation, by_direct,
            "{question}: the two interpreters reported different state"
        ),
        (Err(Unanswered::Domain(by_activation)), Err(Unanswered::Domain(by_direct))) => assert_eq!(
            by_activation.to_string(),
            by_direct.to_string(),
            "{question}: the two interpreters gave different reasons for having no answer"
        ),
        _ => panic!(
            "{question}: the two interpreters answered differently — \
             activation {activation:?}, external executor {direct:?}"
        ),
    }
}

/// The facts a stream holds, decoded as the domain would read them back, paired
/// with the sequence each was given.
async fn recorded_facts(
    store: &Arc<dyn EventStore>,
    ledger: &AggregateId,
) -> Vec<(u64, LedgerEvent)> {
    let loaded: Vec<_> = store
        .load(LoadQuery::by_stream(ledger))
        .await
        .expect("load must succeed")
        .try_collect()
        .await
        .expect("collecting the stream must succeed");

    loaded
        .iter()
        .map(|event| {
            let fact = LedgerEvent::decode(&event.payload)
                .expect("an interpreter must write facts in the format it reads back");
            (event.sequence, fact)
        })
        .collect()
}

/// The same decider, consumed through both interpreters, must leave the domain
/// unable to tell which one carried it out.
///
/// The command sequence is chosen so that every shape of verdict occurs: a
/// creation, an acceptance producing several facts whose order matters, an
/// acceptance producing none, a refusal, and a further acceptance that has to
/// continue from the state the earlier ones left.
#[tokio::test]
async fn one_decider_observed_through_two_interpreters_answers_the_same() {
    // Given: an external executor that already satisfies the laws on its own
    let reports = ReportCapture::install();
    let process_system = ProcessSystem::new().await;
    let activation = ActivationInterpretation::new(process_system, reports);
    let direct = DirectInterpretation;

    verify(&direct).await;

    // And: one stream each, so neither interpreter reads the other's facts
    let activation_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let direct_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let activation_ledger = AggregateId::new("equivalence.ledger.activation");
    let direct_ledger = AggregateId::new("equivalence.ledger.direct");

    let by_activation = activation
        .interpret(activation_ledger.clone(), Arc::clone(&activation_store))
        .await;
    let by_direct = direct
        .interpret(direct_ledger.clone(), Arc::clone(&direct_store))
        .await;

    // When: the same commands and questions reach both, in the same order
    let holder_before = exec_both(&by_activation, &by_direct, || Holder).await;
    let opened = ask_both(&by_activation, &by_direct, || Open {
        holder: HOLDER.to_owned(),
    })
    .await;
    let settled = ask_both(&by_activation, &by_direct, || Settle {
        credit: 10,
        charge: 10,
    })
    .await;
    let nothing_to_do = ask_both(&by_activation, &by_direct, || Settle {
        credit: 0,
        charge: 0,
    })
    .await;
    let underfunded = ask_both(&by_activation, &by_direct, || Settle {
        credit: 0,
        charge: 5,
    })
    .await;
    let settled_again = ask_both(&by_activation, &by_direct, || Settle {
        credit: 5,
        charge: 3,
    })
    .await;
    let balance_after = exec_both(&by_activation, &by_direct, || Balance).await;
    let holder_after = exec_both(&by_activation, &by_direct, || Holder).await;

    // Then: every verdict matched
    assert_same_answer(
        &holder_before.0,
        &holder_before.1,
        "exec(Holder) before the ledger was opened",
    );
    assert_same_verdict(&opened.0, &opened.1, "ask(Open)");
    assert_same_verdict(&settled.0, &settled.1, "ask(Settle) crediting and charging");
    assert_same_verdict(
        &nothing_to_do.0,
        &nothing_to_do.1,
        "ask(Settle) with nothing to credit or charge",
    );
    assert_same_verdict(
        &underfunded.0,
        &underfunded.1,
        "ask(Settle) charging more than the ledger can fund",
    );
    assert_same_verdict(
        &settled_again.0,
        &settled_again.1,
        "ask(Settle) continuing from the state the earlier commands left",
    );
    assert_same_answer(&balance_after.0, &balance_after.1, "exec(Balance)");
    assert_same_answer(&holder_after.0, &holder_after.1, "exec(Holder)");

    // And: the facts left behind matched, in order and under the same numbers
    let activation_facts = recorded_facts(&activation_store, &activation_ledger).await;
    let direct_facts = recorded_facts(&direct_store, &direct_ledger).await;

    assert_eq!(
        activation_facts, direct_facts,
        "the two interpreters must leave the same facts, in the same order, under the same \
         sequence numbers: a domain replaying either stream must reach the same state"
    );
    assert_eq!(
        activation_facts
            .iter()
            .map(|(sequence, _)| *sequence)
            .collect::<Vec<_>>(),
        vec![1, 2, 3, 4, 5],
        "this command sequence produces five facts, numbered consecutively from the genesis \
         sequence; two interpreters that both recorded nothing would otherwise agree vacuously"
    );
}
