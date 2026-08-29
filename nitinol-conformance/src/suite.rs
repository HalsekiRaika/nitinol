use std::fmt::Debug;
use std::sync::Arc;

use futures_util::TryStreamExt;
use jiff::Timestamp;
use nitinol_contract::{Aggregate, Decider, Decision, Event, Query};
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::{AggregateId, AppendingEvent, LoadQuery, LoadedEvent};

use crate::counting_store::{AppendCount, CountingStore};
use crate::fixture::{
    Balance, Holder, Ledger, LedgerEvent, LedgerRejection, MalformedLedgerEvent, Open, Settle,
};
use crate::interpreter::{Interpretation, Interpreter};
use crate::outcome::{Interpreted, Unanswered};
use crate::wedged_store::WedgedStore;

/// Who the ledgers under test are opened for.
///
/// A domain fact, deliberately unrelated to any stream key below.
const HOLDER: &str = "acct-under-test";

/// Put an interpreter through every law of the contract.
///
/// Each clause stages its own ledger — its own store, its own stream key — so
/// no clause can be carried by what another one left behind, and each names the
/// law it found broken when it panics.
///
/// The suite reads the stream back and decodes it itself, so an interpreter is
/// never graded on its own account of what it did.
pub async fn verify<I: Interpretation>(interpretation: &I) {
    decisions_and_answers_are_reproducible(interpretation).await;
    the_facts_of_an_acceptance_land_together_and_in_order(interpretation).await;
    an_acceptance_with_no_facts_still_answers(interpretation).await;
    a_refusal_leaves_no_trace_in_the_stream(interpretation).await;
    an_answer_is_delivered_once_and_a_told_refusal_is_surfaced(interpretation).await;
    a_domain_refusal_is_told_apart_from_a_failure_of_the_machinery(interpretation).await;
    a_creation_that_collides_is_reported_rather_than_answered(interpretation).await;
    sequence_and_time_belong_to_the_machine(interpretation).await;
    the_holder_is_a_domain_fact_and_not_the_stream_key(interpretation).await;
}

/// L-1. The same question, put twice to the same state, comes back with the same
/// answer, and the same command is refused the same way twice.
async fn decisions_and_answers_are_reproducible<I: Interpretation>(interpretation: &I) {
    const LAW: &str = "L-1";

    // Given: a ledger holding something to report
    let under_test = UnderTest::staged(interpretation, "conformance.ledger.determinism", &[]).await;
    delivered(
        LAW,
        "ask(Open)",
        under_test
            .interpreter
            .ask(Open {
                holder: HOLDER.to_owned(),
            })
            .await,
    );
    delivered(
        LAW,
        "ask(Settle) crediting the ledger",
        under_test
            .interpreter
            .ask(Settle {
                credit: 4,
                charge: 0,
            })
            .await,
    );

    // When / Then: asking twice changes nothing about the answer
    let first_balance = reported(
        LAW,
        "exec(Balance)",
        under_test.interpreter.exec(Balance).await,
    );
    let second_balance = reported(
        LAW,
        "exec(Balance)",
        under_test.interpreter.exec(Balance).await,
    );
    assert_eq!(
        first_balance, second_balance,
        "{LAW} is broken: the same question put twice to unchanged state was answered \
         {first_balance} and then {second_balance}, so answering it consulted something \
         other than the state"
    );

    let first_holder = reported(
        LAW,
        "exec(Holder)",
        under_test.interpreter.exec(Holder).await,
    );
    let second_holder = reported(
        LAW,
        "exec(Holder)",
        under_test.interpreter.exec(Holder).await,
    );
    assert_eq!(
        first_holder, second_holder,
        "{LAW} is broken: the same question put twice to unchanged state named \
         {first_holder:?} and then {second_holder:?}"
    );

    // And: deciding twice reaches the same verdict
    let first_refusal = refused(
        LAW,
        "ask(Settle) charging more than the ledger can fund",
        under_test
            .interpreter
            .ask(Settle {
                credit: 0,
                charge: 9,
            })
            .await,
    );
    let second_refusal = refused(
        LAW,
        "ask(Settle) charging more than the ledger can fund",
        under_test
            .interpreter
            .ask(Settle {
                credit: 0,
                charge: 9,
            })
            .await,
    );
    assert_eq!(
        first_refusal, second_refusal,
        "{LAW} is broken: the same command decided twice against unchanged state was refused \
         with {first_refusal:?} and then {second_refusal:?}"
    );
}

/// L-2. The facts of one acceptance reach the stream together, in the order the
/// decision listed them, and a failed append leaves none of them behind.
async fn the_facts_of_an_acceptance_land_together_and_in_order<I: Interpretation>(
    interpretation: &I,
) {
    const LAW: &str = "L-2";

    // Given: a ledger opened and then settled by a command producing two facts
    // that do not commute
    let (under_test, appends) =
        UnderTest::staged_counting(interpretation, "conformance.ledger.ordering", &[]).await;
    delivered(
        LAW,
        "ask(Open)",
        under_test
            .interpreter
            .ask(Open {
                holder: HOLDER.to_owned(),
            })
            .await,
    );
    let appends_before_settle = appends.get();
    let settled = delivered(
        LAW,
        "ask(Settle) crediting and charging",
        under_test
            .interpreter
            .ask(Settle {
                credit: 10,
                charge: 4,
            })
            .await,
    );

    // Then: the two facts that decision produced were committed as one append,
    // not as several that merely happened to land in order — the store's own
    // atomicity contract only holds within a single call
    assert_eq!(
        appends.get(),
        appends_before_settle + 1,
        "{LAW} is broken: the facts of one acceptance must be committed as a single append, not \
         spread across several that happen to land in order"
    );

    // Then: the stream holds exactly those facts, in that order, under one run
    // of sequence numbers
    let recorded = under_test.recorded().await;
    assert_eq!(
        facts(&recorded),
        vec![
            LedgerEvent::Opened {
                holder: HOLDER.to_owned()
            },
            LedgerEvent::Credited { amount: 10 },
            LedgerEvent::Debited { amount: 4 },
        ],
        "{LAW} is broken: the stream must hold the facts each decision listed, in the order it \
         listed them"
    );
    assert_eq!(
        sequences(&recorded),
        vec![1, 2, 3],
        "{LAW} is broken: the facts of one acceptance must be numbered consecutively behind the \
         facts already in the stream"
    );

    // And: replaying that stream reaches the state the interpreter answered from
    let mut replayed = Ledger::default();
    for fact in facts(&recorded) {
        replayed.apply(fact);
    }
    let replayed_balance = replayed
        .query(Balance)
        .expect("a replayed ledger whose stream opens it must report a balance");
    assert_eq!(
        replayed_balance, settled,
        "{LAW} is broken: replaying the stream reached {replayed_balance} while the interpreter \
         answered {settled}, so the recorded order is not the order the decision described"
    );

    // And: an acceptance the store refuses leaves no part of itself behind
    let already_open = [LedgerEvent::Opened {
        holder: HOLDER.to_owned(),
    }];
    let wedged = UnderTest::wedged(
        interpretation,
        "conformance.ledger.atomicity",
        &already_open,
    )
    .await;
    // What the interpreter makes of a store that records nothing is L-6's
    // business; what matters here is that none of the acceptance survived.
    let _ = wedged
        .interpreter
        .ask(Settle {
            credit: 10,
            charge: 4,
        })
        .await;
    assert_eq!(
        facts(&wedged.recorded().await),
        already_open.to_vec(),
        "{LAW} is broken: an acceptance whose append did not go through left part of itself in \
         the stream, so its facts were not written as one unit"
    );
}

/// L-3. A command that finds its work already done is accepted, appends nothing
/// and still answers.
async fn an_acceptance_with_no_facts_still_answers<I: Interpretation>(interpretation: &I) {
    const LAW: &str = "L-3";

    // Given: a ledger with something in it
    let under_test = UnderTest::staged(interpretation, "conformance.ledger.idleness", &[]).await;
    delivered(
        LAW,
        "ask(Open)",
        under_test
            .interpreter
            .ask(Open {
                holder: HOLDER.to_owned(),
            })
            .await,
    );
    let funded = delivered(
        LAW,
        "ask(Settle) crediting the ledger",
        under_test
            .interpreter
            .ask(Settle {
                credit: 7,
                charge: 0,
            })
            .await,
    );
    let before = under_test.recorded().await;

    // When: a settlement with nothing to credit and nothing to charge
    let idle = delivered(
        LAW,
        "ask(Settle) with nothing to credit or charge",
        under_test
            .interpreter
            .ask(Settle {
                credit: 0,
                charge: 0,
            })
            .await,
    );

    // Then: it is answered, and nothing was appended
    assert_eq!(
        idle, funded,
        "{LAW} is broken: an acceptance that produced no facts answered {idle} instead of the \
         {funded} the ledger holds"
    );
    assert_eq!(
        sequences(&under_test.recorded().await),
        sequences(&before),
        "{LAW} is broken: an acceptance that produced no facts still reached the stream"
    );
}

/// L-4. A refusal is accompanied by no persistence whatsoever, and moves no
/// state.
async fn a_refusal_leaves_no_trace_in_the_stream<I: Interpretation>(interpretation: &I) {
    const LAW: &str = "L-4";

    // Given: a ledger funded well below what the next command charges
    let under_test = UnderTest::staged(interpretation, "conformance.ledger.refusal", &[]).await;
    delivered(
        LAW,
        "ask(Open)",
        under_test
            .interpreter
            .ask(Open {
                holder: HOLDER.to_owned(),
            })
            .await,
    );
    delivered(
        LAW,
        "ask(Settle) crediting the ledger",
        under_test
            .interpreter
            .ask(Settle {
                credit: 3,
                charge: 0,
            })
            .await,
    );
    let before = under_test.recorded().await;

    // When: the ledger refuses
    let rejection = refused(
        LAW,
        "ask(Settle) charging more than the ledger can fund",
        under_test
            .interpreter
            .ask(Settle {
                credit: 0,
                charge: 8,
            })
            .await,
    );

    // Then: the refusal says what it is, and the stream and the state are as
    // they were
    assert_eq!(
        rejection,
        LedgerRejection::Underfunded {
            requested: 8,
            available: 3
        },
        "{LAW} is broken: the refusal carried back was not the one the domain rule states"
    );

    let after = under_test.recorded().await;
    assert_eq!(
        facts(&after),
        facts(&before),
        "{LAW} is broken: a refused command left facts in the stream"
    );
    assert_eq!(
        sequences(&after),
        sequences(&before),
        "{LAW} is broken: a refused command consumed a sequence number"
    );

    let balance = reported(
        LAW,
        "exec(Balance)",
        under_test.interpreter.exec(Balance).await,
    );
    assert_eq!(
        balance, 3,
        "{LAW} is broken: a refused command moved the state to {balance}"
    );
}

/// L-5. The ask path delivers the decision's answer, once and freshly; the tell
/// path drops the answer but surfaces a refusal rather than losing it.
async fn an_answer_is_delivered_once_and_a_told_refusal_is_surfaced<I: Interpretation>(
    interpretation: &I,
) {
    const LAW: &str = "L-5";

    // Given: a ledger, and a model of it the suite keeps for itself
    let under_test = UnderTest::staged(interpretation, "conformance.ledger.delivery", &[]).await;
    let mut model = Ledger::default();
    delivered(
        LAW,
        "ask(Open)",
        under_test
            .interpreter
            .ask(Open {
                holder: HOLDER.to_owned(),
            })
            .await,
    );
    foretell(
        &mut model,
        Open {
            holder: HOLDER.to_owned(),
        },
    )
    .expect("opening a ledger nobody has opened is accepted");

    // When: the same command is asked twice
    let first = delivered(
        LAW,
        "ask(Settle) crediting the ledger",
        under_test
            .interpreter
            .ask(Settle {
                credit: 5,
                charge: 0,
            })
            .await,
    );
    let foretold_first = foretell(
        &mut model,
        Settle {
            credit: 5,
            charge: 0,
        },
    )
    .expect("crediting an open ledger is accepted");
    let second = delivered(
        LAW,
        "ask(Settle) crediting the ledger again",
        under_test
            .interpreter
            .ask(Settle {
                credit: 5,
                charge: 0,
            })
            .await,
    );
    let foretold_second = foretell(
        &mut model,
        Settle {
            credit: 5,
            charge: 0,
        },
    )
    .expect("crediting an open ledger is accepted");

    // Then: each ask carried its own decision's answer
    assert_eq!(
        first, foretold_first,
        "{LAW} is broken: the first ask answered {first} where the decision states {foretold_first}"
    );
    assert_eq!(
        second, foretold_second,
        "{LAW} is broken: the second ask answered {second} where the decision states \
         {foretold_second}"
    );
    assert_ne!(
        first, second,
        "{LAW} is broken: the second ask repeated the first ask's answer instead of delivering \
         the answer its own decision reached"
    );

    // When: a command nobody is waiting for is refused
    let expected = foretell(
        &mut model,
        Settle {
            credit: 0,
            charge: 99,
        },
    )
    .expect_err("charging far more than an open ledger holds is refused")
    .to_string();
    let before = under_test.recorded().await;
    under_test
        .interpreter
        .tell(Settle {
            credit: 0,
            charge: 99,
        })
        .await
        .expect("the machinery must take a told command");
    under_test.interpreter.quiesce().await;

    // Then: the refusal was surfaced, and nothing was written
    let surfaced = under_test.interpreter.surfaced_refusals();
    assert!(
        surfaced.iter().any(|report| report.contains(&expected)),
        "{LAW} is broken: a told command was refused and the refusal was not surfaced anywhere \
         naming {expected:?}; what was surfaced is {surfaced:?}"
    );
    assert_eq!(
        sequences(&under_test.recorded().await),
        sequences(&before),
        "{LAW} is broken: a told command that was refused still reached the stream"
    );

    // When: a command nobody is waiting for is accepted
    let recorded_before = under_test.recorded().await.len();
    under_test
        .interpreter
        .tell(Settle {
            credit: 2,
            charge: 1,
        })
        .await
        .expect("the machinery must take a told command");
    under_test.interpreter.quiesce().await;

    // Then: its facts were written and no refusal was invented
    assert_eq!(
        under_test.interpreter.surfaced_refusals().len(),
        surfaced.len(),
        "{LAW} is broken: a told command that was accepted surfaced a refusal"
    );
    assert_eq!(
        under_test.recorded().await.len(),
        recorded_before + 2,
        "{LAW} is broken: a told command that was accepted must still have its facts recorded"
    );
}

/// L-6. A rejection carries domain-rule violations only; a failure of the
/// machinery is reported as one.
async fn a_domain_refusal_is_told_apart_from_a_failure_of_the_machinery<I: Interpretation>(
    interpretation: &I,
) {
    const LAW: &str = "L-6";

    // Given: a ledger nobody has opened
    let by_the_rules = UnderTest::staged(interpretation, "conformance.ledger.verdict", &[]).await;

    // When / Then: settling it breaks a domain rule, and is a verdict
    match by_the_rules
        .interpreter
        .ask(Settle {
            credit: 0,
            charge: 1,
        })
        .await
    {
        Interpreted::Refused(LedgerRejection::NotOpen) => {}
        other => panic!(
            "{LAW} is broken: settling a ledger nobody has opened is a domain rule refusing the \
             command, and the interpreter reported {other:?}"
        ),
    }

    // Given: a ledger whose store answers reads and records nothing
    let wedged = UnderTest::wedged(interpretation, "conformance.ledger.breakage", &[]).await;

    // When / Then: the command is accepted by the domain and never recorded,
    // which is not a verdict on it
    match wedged
        .interpreter
        .ask(Open {
            holder: HOLDER.to_owned(),
        })
        .await
    {
        Interpreted::Failed(_) => {}
        other => panic!(
            "{LAW} is broken: a store that refuses to record says nothing about the command, and \
             the interpreter reported {other:?}"
        ),
    }
}

/// L-7. A creation that collides with a ledger that already exists is reported
/// as the collision, not answered as a success.
async fn a_creation_that_collides_is_reported_rather_than_answered<I: Interpretation>(
    interpretation: &I,
) {
    const LAW: &str = "L-7";
    const CREATED_BY_SOMEONE_ELSE: &str = "acct-elsewhere";

    // Given: an interpreter that has finished reading an empty history
    let under_test = UnderTest::staged(interpretation, "conformance.ledger.collision", &[]).await;
    // Without this the interpreter may still read the history after the writer
    // below has written to it, and would then decide from the creation instead
    // of colliding with it.
    under_test.interpreter.quiesce().await;

    // And: another writer creates the ledger behind its back
    let created = [LedgerEvent::Opened {
        holder: CREATED_BY_SOMEONE_ELSE.to_owned(),
    }];
    under_test.write_behind(&created).await;

    // When / Then: the creation collides, and no answer is invented for it
    match under_test
        .interpreter
        .ask(Open {
            holder: HOLDER.to_owned(),
        })
        .await
    {
        Interpreted::AlreadyCreated => {}
        other => panic!(
            "{LAW} is broken: creating a ledger someone else has already created reached no \
             decision, and the interpreter reported {other:?}"
        ),
    }

    // And: the collision left the first creation as it was
    let recorded = under_test.recorded().await;
    assert_eq!(
        facts(&recorded),
        created.to_vec(),
        "{LAW} is broken: a creation that collided still wrote to the stream"
    );
    assert_eq!(
        sequences(&recorded),
        vec![1],
        "{LAW} is broken: a creation that collided consumed a sequence number"
    );
}

/// L-8. `sequence` and `occurred_at` are the machine's coordinates: it assigns
/// them, and a domain that has neither still reaches every answer.
async fn sequence_and_time_belong_to_the_machine<I: Interpretation>(interpretation: &I) {
    const LAW: &str = "L-8";

    // Given: a ledger, and a model of it that holds no sequence and no clock
    let under_test = UnderTest::staged(interpretation, "conformance.ledger.coordinates", &[]).await;
    let mut model = Ledger::default();
    delivered(
        LAW,
        "ask(Open)",
        under_test
            .interpreter
            .ask(Open {
                holder: HOLDER.to_owned(),
            })
            .await,
    );
    foretell(
        &mut model,
        Open {
            holder: HOLDER.to_owned(),
        },
    )
    .expect("opening a ledger nobody has opened is accepted");

    // When: a run of settlements
    for (credit, charge) in [(6u64, 2u64), (0, 0), (1, 5)] {
        let answered = delivered(
            LAW,
            "ask(Settle)",
            under_test.interpreter.ask(Settle { credit, charge }).await,
        );
        let foretold = foretell(&mut model, Settle { credit, charge })
            .expect("a settlement the ledger can fund is accepted");

        // Then: the model reached the same answer without either coordinate
        assert_eq!(
            answered, foretold,
            "{LAW} is broken: the interpreter answered {answered} where a ledger holding neither \
             a sequence nor a clock reaches {foretold}, so an answer depended on the machine's \
             own coordinates"
        );
    }

    // And: the machine numbered and stamped what it recorded
    let recorded = under_test.recorded().await;
    assert_eq!(
        sequences(&recorded),
        vec![1, 2, 3, 4, 5],
        "{LAW} is broken: the stream must be numbered consecutively from the genesis sequence"
    );
    for event in &recorded {
        assert_ne!(
            event.occurred_at,
            Timestamp::UNIX_EPOCH,
            "{LAW} is broken: a recorded fact carries no time of its own, so the interpreter \
             assigned none"
        );
    }
}

/// L-9. The holder is a fact the ledger received through its creation event, not
/// a handle the machinery passed in.
async fn the_holder_is_a_domain_fact_and_not_the_stream_key<I: Interpretation>(interpretation: &I) {
    const LAW: &str = "L-9";
    const HOLDER_OF_RECORD: &str = "acct-42";

    // Given: a ledger nobody has opened
    let under_test = UnderTest::staged(interpretation, "conformance.ledger.identity", &[]).await;

    // When / Then: it has no holder to name yet
    match under_test.interpreter.exec(Holder).await {
        Err(Unanswered::Domain(_)) => {}
        other => panic!(
            "{LAW} is broken: a ledger nobody has opened has received no holder, and the \
             interpreter answered {other:?}"
        ),
    }

    // When: it is opened for a holder unrelated to its stream key
    delivered(
        LAW,
        "ask(Open)",
        under_test
            .interpreter
            .ask(Open {
                holder: HOLDER_OF_RECORD.to_owned(),
            })
            .await,
    );

    // Then: the holder it names is the one the creation carried
    let holder = reported(
        LAW,
        "exec(Holder)",
        under_test.interpreter.exec(Holder).await,
    );
    assert_eq!(
        holder, HOLDER_OF_RECORD,
        "{LAW} is broken: the ledger named {holder:?} as its holder rather than the one its \
         creation carried"
    );
    assert_ne!(
        holder,
        under_test.ledger.as_str(),
        "{LAW} is broken: the ledger answered with the key of the stream it persists to, which \
         the machinery handed it rather than the domain"
    );
}

// Staging a clause

/// One clause's ledger: the interpreter under test, the stream key it was
/// brought up for, and the store the suite reads that stream back from.
struct UnderTest<I: Interpretation> {
    interpreter: I::Interpreter,
    ledger: AggregateId,
    /// Where the suite reads from. For a wedged clause this is the store
    /// *underneath* the wedge, so facts seeded before the wedge stay visible
    /// even though the interpreter can record nothing through it.
    recorded_by: Arc<dyn EventStore>,
}

impl<I: Interpretation> UnderTest<I> {
    /// Bring an interpreter up for a fresh stream that already holds `history`.
    async fn staged(interpretation: &I, ledger: &str, history: &[LedgerEvent]) -> Self {
        let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
        let ledger = AggregateId::new(ledger);
        seed(&store, &ledger, history).await;
        let interpreter = interpretation
            .interpret(ledger.clone(), Arc::clone(&store))
            .await;
        Self {
            interpreter,
            ledger,
            recorded_by: store,
        }
    }

    /// Bring an interpreter up for a fresh stream that already holds
    /// `history`, over a store that counts how many times it is asked to
    /// append.
    ///
    /// Seeding `history` goes through the plain backing store first, the same
    /// way [`Self::wedged`] does, so the count returned only ever reflects
    /// what the interpreter itself asked for.
    async fn staged_counting(
        interpretation: &I,
        ledger: &str,
        history: &[LedgerEvent],
    ) -> (Self, AppendCount) {
        let backing: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
        let ledger = AggregateId::new(ledger);
        seed(&backing, &ledger, history).await;
        let (counting, appends) = CountingStore::over(Arc::clone(&backing));
        let counting: Arc<dyn EventStore> = Arc::new(counting);
        let interpreter = interpretation.interpret(ledger.clone(), counting).await;
        (
            Self {
                interpreter,
                ledger,
                recorded_by: backing,
            },
            appends,
        )
    }

    /// Bring an interpreter up over a store that answers reads and records
    /// nothing, on a stream that already holds `history`.
    async fn wedged(interpretation: &I, ledger: &str, history: &[LedgerEvent]) -> Self {
        let backing: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
        let ledger = AggregateId::new(ledger);
        seed(&backing, &ledger, history).await;
        let wedged: Arc<dyn EventStore> = Arc::new(WedgedStore::over(Arc::clone(&backing)));
        let interpreter = interpretation.interpret(ledger.clone(), wedged).await;
        Self {
            interpreter,
            ledger,
            recorded_by: backing,
        }
    }

    /// Everything the stream holds, in the order a replay would read it.
    async fn recorded(&self) -> Vec<LoadedEvent> {
        self.recorded_by
            .load(LoadQuery::by_stream(&self.ledger))
            .await
            .expect("the suite's own store must answer a load")
            .try_collect()
            .await
            .expect("the suite's own store must not fail part-way through a stream")
    }

    /// Write `facts` to the stream as a writer the interpreter knows nothing
    /// about.
    async fn write_behind(&self, facts: &[LedgerEvent]) {
        seed(&self.recorded_by, &self.ledger, facts).await;
    }
}

/// Write `facts` to an empty stream, numbered from the genesis sequence.
///
/// Only ever called on a stream nothing has been written to yet, which is what
/// makes numbering from one correct.
async fn seed(store: &Arc<dyn EventStore>, ledger: &AggregateId, facts: &[LedgerEvent]) {
    let appending: Vec<AppendingEvent> = facts
        .iter()
        .enumerate()
        .map(|(index, fact)| AppendingEvent {
            sequence: index as u64 + 1,
            event_type: fact.variant(),
            payload: fact.encode(),
            occurred_at: Timestamp::now(),
        })
        .collect();

    store
        .append(ledger.as_str(), appending)
        .await
        .expect("the suite's own store must take the history the suite seeds");
}

/// The facts a stream holds, as the domain would read them back.
fn facts(recorded: &[LoadedEvent]) -> Vec<LedgerEvent> {
    recorded
        .iter()
        .map(|event| {
            LedgerEvent::decode(&event.payload).unwrap_or_else(|malformed: MalformedLedgerEvent| {
                panic!("an interpreter must record facts in the format it was handed: {malformed}")
            })
        })
        .collect()
}

/// The sequence each fact was given, in the order they are read back.
fn sequences(recorded: &[LoadedEvent]) -> Vec<u64> {
    recorded.iter().map(|event| event.sequence).collect()
}

// Reading a verdict

/// What the contract alone says `cmd` means for `model`, advancing the model by
/// the facts the decision states.
///
/// The model holds no identity, no sequence and no clock, so an answer it
/// reaches is one the domain could have reached without any machinery at all.
fn foretell<C>(
    model: &mut Ledger,
    cmd: C,
) -> Result<<Ledger as Decider<C>>::Output, LedgerRejection>
where
    Ledger: Decider<C, Rejection = LedgerRejection>,
{
    match model.decide(cmd) {
        Decision::Accept { events, output } => {
            for event in events {
                model.apply(event);
            }
            Ok(output)
        }
        Decision::Reject(rejection) => Err(rejection),
    }
}

/// The answer an acceptance owed, or a panic naming the law that was broken.
fn delivered<O: Debug>(law: &str, command: &str, verdict: Interpreted<O>) -> O {
    match verdict {
        Interpreted::Answered(output) => output,
        other => panic!(
            "{law} is broken: {command} is accepted by the ledger, and the interpreter reported \
             {other:?} instead of delivering the answer"
        ),
    }
}

/// The rejection a refusal carried, or a panic naming the law that was broken.
fn refused<O: Debug>(law: &str, command: &str, verdict: Interpreted<O>) -> LedgerRejection {
    match verdict {
        Interpreted::Refused(rejection) => rejection,
        other => panic!(
            "{law} is broken: {command} is refused by a domain rule, and the interpreter reported \
             {other:?} instead"
        ),
    }
}

/// The answer to a question, or a panic naming the law that was broken.
fn reported<R: Debug>(law: &str, question: &str, answer: Result<R, Unanswered>) -> R {
    match answer {
        Ok(response) => response,
        Err(unanswered) => panic!(
            "{law} is broken: the ledger can answer {question} in this state, and the interpreter \
             reported {unanswered:?}"
        ),
    }
}
