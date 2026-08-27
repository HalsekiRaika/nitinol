//! Acceptance scenario "runtime-free での契約実装": with `nitinol-contract` and
//! `nitinol-persistence` alone — no async runtime, no `async_trait`, no
//! `#[tokio::test]` anywhere in this file — a domain crate must be able to
//! implement `Decider<C>` and `Query<M>` and unit- and property-test them.
//!
//! Reaching for `nitinol-eventsource`, `nitinol-runtime`, `tokio` or `futures`
//! here would silently void the scenario: the whole point of the contract crate
//! is that the decision and the question are expressible without the machinery
//! that later interprets them.
//!
//! Run with: `cargo test -p nitinol-contract --test decider_query`.

use std::rc::Rc;

use nitinol_contract::{Aggregate, Decider, Decision, Event, Query};
use nitinol_persistence::{EventType, Family, TypeName};

// Fixture: a wallet that holds a balance and an optional label.

#[derive(Clone, Debug, PartialEq, Eq)]
enum WalletEvent {
    Credited { amount: u64 },
    Debited { amount: u64 },
    Labelled { label: String },
}

// Written by hand rather than derived: `nitinol-contract` does not depend on
// `nitinol-macros`, and depending on it here would widen the dependency set the
// scenario is about.
impl Event for WalletEvent {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("shop.wallet"), TypeName::new("WalletEvent"));
}

#[derive(Debug, Default)]
struct Wallet {
    balance: u64,
    label: Option<String>,
}

impl Aggregate for Wallet {
    type Event = WalletEvent;

    fn apply(&mut self, event: WalletEvent) {
        match event {
            WalletEvent::Credited { amount } => self.balance = self.balance.saturating_add(amount),
            WalletEvent::Debited { amount } => self.balance = self.balance.saturating_sub(amount),
            WalletEvent::Labelled { label } => self.label = Some(label),
        }
    }
}

/// Credit the wallet, then charge it — in that order, which the wallet's
/// saturating arithmetic makes observable.
struct Settle {
    credit: u64,
    charge: u64,
}

/// Attach a label the caller owns and shares.
///
/// `Rc<str>` is neither `Send` nor `Sync`: this impl only compiles while
/// `Decider<C>` leaves `C` unbounded. `nitinol_eventsource::Decider` requires
/// `C: Send + Sync + 'static`, and inheriting that bound is the most likely way
/// to get the new contract subtly wrong.
struct Label(Rc<str>);

/// Domain-rule violations only (L-6).
///
/// Deliberately does *not* implement `std::error::Error`:
/// `nitinol_eventsource::Decider` requires
/// `Rejection: std::error::Error + Send + Sync + 'static`, and the new contract
/// must not. If that bound reappears, this file stops compiling.
#[derive(Debug, PartialEq, Eq)]
enum WalletRejection {
    InsufficientFunds { requested: u64, available: u64 },
    EmptyLabel,
}

impl Decider<Settle> for Wallet {
    type Output = u64;
    type Rejection = WalletRejection;

    fn decide(&self, cmd: Settle) -> Decision<WalletEvent, u64, WalletRejection> {
        let funded = self.balance.saturating_add(cmd.credit);
        if cmd.charge > funded {
            return Decision::reject(WalletRejection::InsufficientFunds {
                requested: cmd.charge,
                available: funded,
            });
        }

        let mut events = Vec::new();
        if cmd.credit > 0 {
            events.push(WalletEvent::Credited { amount: cmd.credit });
        }
        if cmd.charge > 0 {
            events.push(WalletEvent::Debited { amount: cmd.charge });
        }

        Decision::persist(events).output(funded - cmd.charge)
    }
}

impl Decider<Label> for Wallet {
    // A command that needs no answer says so once, here — not per decision.
    type Output = ();
    type Rejection = WalletRejection;

    fn decide(&self, cmd: Label) -> Decision<WalletEvent, (), WalletRejection> {
        if cmd.0.is_empty() {
            return Decision::reject(WalletRejection::EmptyLabel);
        }
        if self.label.as_deref() == Some(&*cmd.0) {
            return Decision::persist(Vec::new()).output(());
        }
        Decision::persist(vec![WalletEvent::Labelled {
            label: cmd.0.to_string(),
        }])
        .output(())
    }
}

/// Ask for this wallet's label, qualified by a namespace the caller owns and
/// shares. `Rc<str>` again: `Query<M>` must leave `M` unbounded, where
/// `nitinol_eventsource::Receive` requires `M: Send + Sync + 'static`.
struct QualifiedLabel(Rc<str>);

/// Like [`WalletRejection`], deliberately not a `std::error::Error`:
/// `nitinol_eventsource::Receive` constrains its `Error`, and this one is
/// unconstrained.
#[derive(Debug, PartialEq, Eq)]
enum WalletQueryError {
    Unlabelled,
}

impl Query<QualifiedLabel> for Wallet {
    // `Rc<str>` is not `Send`: `Receive::Response` is `Send + Sync + 'static`,
    // `Query::Response` is unbounded.
    type Response = Rc<str>;
    type Error = WalletQueryError;

    fn query(&self, msg: QualifiedLabel) -> Result<Rc<str>, WalletQueryError> {
        match &self.label {
            Some(label) => Ok(Rc::from(format!("{}.{label}", msg.0))),
            None => Err(WalletQueryError::Unlabelled),
        }
    }
}

// The `Decision` vocabulary

/// Given events and an answer, When they are threaded through the builder,
/// Then the completed value is the `Accept` variant carrying both — in the
/// order they were handed over.
#[test]
fn persist_then_output_completes_into_accept() {
    let events = vec![
        WalletEvent::Credited { amount: 10 },
        WalletEvent::Debited { amount: 4 },
    ];

    let decision: Decision<WalletEvent, u64, WalletRejection> =
        Decision::persist(events.clone()).output(6);

    assert_eq!(decision, Decision::Accept { events, output: 6 });
}

/// Given a domain-rule violation, When it is passed to the builder, Then the
/// value is the `Reject` variant and carries nothing but the rejection — there
/// is no output to supply and no events to append (L-4).
#[test]
fn reject_completes_without_events_or_output() {
    let decision =
        Decision::<WalletEvent, u64, WalletRejection>::reject(WalletRejection::InsufficientFunds {
            requested: 20,
            available: 15,
        });

    assert_eq!(
        decision,
        Decision::Reject(WalletRejection::InsufficientFunds {
            requested: 20,
            available: 15,
        })
    );
}

// `Decider<C>`

/// Given a funded wallet, When a settlement is decided, Then the decision
/// states the facts and answers the question in one value.
#[test]
fn an_accepted_command_states_the_facts_and_answers() {
    let wallet = Wallet {
        balance: 100,
        ..Wallet::default()
    };

    let decision = wallet.decide(Settle {
        credit: 0,
        charge: 30,
    });

    assert_eq!(
        decision,
        Decision::Accept {
            events: vec![WalletEvent::Debited { amount: 30 }],
            output: 70,
        }
    );
}

/// Given a decision whose events do not commute, When they are applied in the
/// order the decision listed them, Then the state reaches exactly what the
/// output promised — and applying them in any other order does not (L-2).
#[test]
fn accepted_events_reach_the_promised_state_only_in_the_stated_order() {
    let wallet = Wallet::default();

    let decision = wallet.decide(Settle {
        credit: 10,
        charge: 10,
    });

    let Decision::Accept { events, output } = decision else {
        panic!("a fully funded settlement must be accepted");
    };
    assert_eq!(
        events,
        vec![
            WalletEvent::Credited { amount: 10 },
            WalletEvent::Debited { amount: 10 },
        ]
    );

    let mut in_order = Wallet::default();
    for event in events.iter().cloned() {
        in_order.apply(event);
    }
    assert_eq!(in_order.balance, output);

    let mut reversed = Wallet::default();
    for event in events.into_iter().rev() {
        reversed.apply(event);
    }
    assert_ne!(
        reversed.balance, output,
        "the fixture must be order-sensitive, otherwise the assertion above \
         would accept a decision that lists its events backwards"
    );
}

/// Given a command that changes nothing, When it is decided, Then acceptance
/// with an empty event list is a legitimate outcome: nothing is appended, and
/// the answer is delivered as usual (L-3).
#[test]
fn idempotent_acceptance_appends_nothing_and_still_answers() {
    let wallet = Wallet {
        balance: 42,
        ..Wallet::default()
    };

    let decision = wallet.decide(Settle {
        credit: 0,
        charge: 0,
    });

    assert_eq!(
        decision,
        Decision::Accept {
            events: Vec::new(),
            output: 42,
        }
    );
}

/// Given a decider that has nothing to answer, When it accepts, Then its
/// output is the unit value — declared once on the impl, not made optional per
/// decision — and a repeated command is idempotent acceptance (L-3).
#[test]
fn a_decider_with_nothing_to_answer_outputs_unit() {
    let mut wallet = Wallet::default();

    let decision = wallet.decide(Label(Rc::from("payroll")));
    assert_eq!(
        decision,
        Decision::Accept {
            events: vec![WalletEvent::Labelled {
                label: "payroll".to_owned(),
            }],
            output: (),
        }
    );

    let Decision::Accept { events, .. } = decision else {
        panic!("labelling an unlabelled wallet must be accepted");
    };
    for event in events {
        wallet.apply(event);
    }
    assert_eq!(
        wallet.label.as_deref(),
        Some("payroll"),
        "applying the decided events must produce the state the decision described, got {wallet:?}"
    );

    let repeated = wallet.decide(Label(Rc::from("payroll")));
    assert_eq!(
        repeated,
        Decision::Accept {
            events: Vec::new(),
            output: (),
        }
    );
}

/// Given a command that violates a domain rule, When it is decided, Then the
/// decision is a rejection that names the violation, and no events exist to be
/// persisted (L-4).
#[test]
fn a_rejected_command_names_the_violation_and_produces_no_events() {
    let wallet = Wallet {
        balance: 10,
        ..Wallet::default()
    };

    let decision = wallet.decide(Settle {
        credit: 5,
        charge: 20,
    });

    assert_eq!(
        decision,
        Decision::Reject(WalletRejection::InsufficientFunds {
            requested: 20,
            available: 15,
        })
    );
}

/// Given an empty label, When it is decided by the unit-output decider, Then
/// rejection is reachable regardless of what `Output` was declared to be.
#[test]
fn a_unit_output_decider_can_still_reject() {
    let wallet = Wallet::default();

    let decision = wallet.decide(Label(Rc::from("")));

    assert_eq!(decision, Decision::Reject(WalletRejection::EmptyLabel));
}

// `Query<M>`

/// Given a labelled and an unlabelled wallet, When the same question is asked
/// of each, Then the answer comes from state alone and the failure is the
/// domain error the impl declared — never an event, never a decision.
#[test]
fn a_query_answers_from_state_and_reports_its_domain_error() {
    let labelled = Wallet {
        label: Some("payroll".to_owned()),
        ..Wallet::default()
    };
    assert_eq!(
        labelled.query(QualifiedLabel(Rc::from("acme"))),
        Ok(Rc::from("acme.payroll")),
    );

    let unlabelled = Wallet::default();
    assert_eq!(
        unlabelled.query(QualifiedLabel(Rc::from("acme"))),
        Err(WalletQueryError::Unlabelled),
    );
}

// Properties (L-1, L-2, L-4) — deterministic, and built without an async
// runtime or a randomness crate.

/// A fixed-seed linear congruential generator.
///
/// `nitinol-contract` has no dev-dependencies, and pulling `proptest` or `rand`
/// in to reach a property test would weaken the very claim under test: that the
/// contract is exercisable with nothing but runtime-free crates.
struct Lcg(u64);

impl Lcg {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    /// Knuth's MMIX constants.
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0
    }

    /// The low bits of an LCG cycle with a very short period, so sample from
    /// the high half.
    fn below(&mut self, bound: u64) -> u64 {
        (self.next() >> 32) % bound
    }
}

const CASES: usize = 512;

/// One generated case: a wallet state plus the amounts a settlement would use.
struct Case {
    balance: u64,
    labelled: bool,
    credit: u64,
    charge: u64,
}

impl Case {
    fn generate(lcg: &mut Lcg) -> Self {
        Self {
            balance: lcg.below(64),
            labelled: lcg.below(2) == 1,
            credit: lcg.below(64),
            // Reaches past `balance + credit`, so both the accepting and the
            // rejecting class are sampled.
            charge: lcg.below(160),
        }
    }

    fn wallet(&self) -> Wallet {
        Wallet {
            balance: self.balance,
            label: self.labelled.then(|| "payroll".to_owned()),
        }
    }
}

/// For any state and command, deciding twice yields the same decision: no
/// clock, no randomness, no hidden state (L-1).
#[test]
fn deciding_twice_yields_the_same_decision() {
    let mut lcg = Lcg::new(0x5EED_0001);

    for _ in 0..CASES {
        let case = Case::generate(&mut lcg);
        let wallet = case.wallet();

        let first = wallet.decide(Settle {
            credit: case.credit,
            charge: case.charge,
        });
        let second = wallet.decide(Settle {
            credit: case.credit,
            charge: case.charge,
        });

        assert_eq!(
            first, second,
            "decide is not deterministic for balance={}, credit={}, charge={}",
            case.balance, case.credit, case.charge,
        );
    }
}

/// For any state and message, asking twice yields the same answer (L-1).
#[test]
fn asking_twice_yields_the_same_answer() {
    let mut lcg = Lcg::new(0x5EED_0002);

    for _ in 0..CASES {
        let case = Case::generate(&mut lcg);
        let wallet = case.wallet();

        let first = wallet.query(QualifiedLabel(Rc::from("acme")));
        let second = wallet.query(QualifiedLabel(Rc::from("acme")));

        assert_eq!(
            first, second,
            "query is not deterministic for balance={}, labelled={}",
            case.balance, case.labelled,
        );
    }
}

/// For any accepted decision, applying the events in the order the decision
/// listed them reaches the state the output described (L-2).
#[test]
fn accepted_events_always_replay_into_the_answer() {
    let mut lcg = Lcg::new(0x5EED_0003);
    let mut accepted = 0usize;

    for _ in 0..CASES {
        let case = Case::generate(&mut lcg);
        let decision = case.wallet().decide(Settle {
            credit: case.credit,
            charge: case.charge,
        });

        let Decision::Accept { events, output } = decision else {
            continue;
        };
        accepted += 1;

        let mut replayed = case.wallet();
        for event in events {
            replayed.apply(event);
        }
        assert_eq!(
            replayed.balance, output,
            "replaying the decided events did not reach the answered balance \
             for balance={}, credit={}, charge={}",
            case.balance, case.credit, case.charge,
        );
    }

    assert!(
        accepted > 0,
        "no case was accepted, so the property held vacuously"
    );
}

/// A settlement is rejected exactly when the wallet cannot fund it, and a
/// rejection never carries events to append (L-4).
#[test]
fn underfunded_settlements_are_always_rejected() {
    let mut lcg = Lcg::new(0x5EED_0004);
    let mut rejected = 0usize;

    for _ in 0..CASES {
        let case = Case::generate(&mut lcg);
        let funded = case.balance + case.credit;
        let decision = case.wallet().decide(Settle {
            credit: case.credit,
            charge: case.charge,
        });

        if case.charge > funded {
            rejected += 1;
            assert_eq!(
                decision,
                Decision::Reject(WalletRejection::InsufficientFunds {
                    requested: case.charge,
                    available: funded,
                }),
                "an underfunded settlement must reject and name the amounts \
                 for balance={}, credit={}, charge={}",
                case.balance,
                case.credit,
                case.charge,
            );
        } else {
            assert!(
                matches!(decision, Decision::Accept { .. }),
                "a funded settlement must be accepted for balance={}, \
                 credit={}, charge={}",
                case.balance,
                case.credit,
                case.charge,
            );
        }
    }

    assert!(
        rejected > 0,
        "no case was rejected, so the property held vacuously"
    );
}

// Compile-fail coverage
//
// "a `Decision` cannot be produced without `.output(..)`" is a type-level
// property: there is no value to assert on, only a program that must fail to
// compile. It is verified by a `compile_fail` rustdoc doctest next to the
// builder, paired with an adjacent passing example so an unrelated compile
// error cannot make it succeed vacuously.
// See: nitinol-contract/src/decision.rs
