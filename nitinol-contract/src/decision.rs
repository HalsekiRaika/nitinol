use std::marker::PhantomData;

/// What a [`Decider`](crate::Decider) concluded about one command.
///
/// A decision is a value, not an effect. It states the facts that follow from
/// the command together with the answer the caller asked for, or it states that
/// a domain rule refused the command. Appending the events and delivering the
/// answer belong to whichever interpreter runs the decider, under the laws in
/// the [crate documentation](crate).
///
/// `Accept` carries the facts and the answer together because they are one
/// conclusion: an interpreter handed events without an answer would have to
/// invent one. `Reject` carries neither, because a refused command produces no
/// facts to append (L-4) and the rejection value says everything there is to
/// say about the refusal (L-6).
///
/// Values are built through [`Decision::persist`] and [`Decision::reject`].
#[derive(Debug, PartialEq)]
pub enum Decision<E, O, R> {
    /// The command was accepted.
    ///
    /// `events` are the facts it produced, listed in the order
    /// [`Aggregate::apply`](crate::Aggregate::apply) must receive them (L-2).
    /// An empty list is a legitimate acceptance — the command found nothing
    /// left to do, and `output` is still the answer (L-3).
    Accept { events: Vec<E>, output: O },
    /// A domain rule refused the command.
    Reject(R),
}

impl<E, O, R> Decision<E, O, R> {
    /// State the facts the command produced, in the order `apply` must receive
    /// them (L-2), and continue to [`Accepting::output`] for the answer.
    ///
    /// Passing an empty `Vec` is how a command that found nothing left to do is
    /// accepted without appending anything (L-3).
    ///
    /// # The answer cannot be forgotten
    ///
    /// `persist` alone yields an [`Accepting`], which is not a `Decision`.  Only
    /// [`output`][Accepting::output] completes one, so a decider that states
    /// facts but never answers does not compile:
    ///
    /// ```rust
    /// use nitinol_contract::Decision;
    ///
    /// struct Refused;
    ///
    /// let decision: Decision<&str, u64, Refused> =
    ///     Decision::persist(vec!["credited"]).output(10);
    ///
    /// assert!(matches!(decision, Decision::Accept { output: 10, .. }));
    /// ```
    ///
    /// ```compile_fail
    /// use nitinol_contract::Decision;
    ///
    /// struct Refused;
    ///
    /// // Compile error: `persist` returns `Accepting`, and without `.output(..)`
    /// // the answer is missing, so no `Decision` exists to bind here.
    /// let decision: Decision<&str, u64, Refused> = Decision::persist(vec!["credited"]);
    /// ```
    pub fn persist(events: Vec<E>) -> Accepting<E, O, R> {
        Accepting {
            events,
            completed: PhantomData,
        }
    }

    /// Refuse the command on a domain rule.
    ///
    /// There is nothing further to supply: the refusal produces no events to
    /// append (L-4) and no answer to deliver.
    ///
    /// ```rust
    /// use nitinol_contract::Decision;
    ///
    /// #[derive(Debug, PartialEq)]
    /// struct InsufficientFunds;
    ///
    /// let decision = Decision::<&str, u64, _>::reject(InsufficientFunds);
    ///
    /// assert_eq!(decision, Decision::Reject(InsufficientFunds));
    /// ```
    pub fn reject(rejection: R) -> Self {
        Decision::Reject(rejection)
    }
}

/// An acceptance whose facts are stated but whose answer is not.
///
/// Produced by [`Decision::persist`] and turned into a [`Decision`] by
/// [`output`][Accepting::output]. It exists so that "which events happened" and
/// "what the caller is told" are supplied by the same expression: a decider
/// cannot return this type where a `Decision` is expected.
#[must_use = "an `Accepting` is not a decision until `output` states the answer"]
pub struct Accepting<E, O, R> {
    events: Vec<E>,
    // `fn() -> (O, R)` rather than `(O, R)`: the builder holds neither value, so
    // its auto traits and drop check must not be constrained by them. The
    // parameters are carried at all so that the return type of `persist` names
    // them and the decision type infers from the decider's signature.
    completed: PhantomData<fn() -> (O, R)>,
}

impl<E, O, R> Accepting<E, O, R> {
    /// Answer the question the command asked, completing the decision.
    ///
    /// A command that asks nothing still answers: its decider declares
    /// `type Output = ()` once, and hands `()` here.
    pub fn output(self, output: O) -> Decision<E, O, R> {
        Decision::Accept {
            events: self.events,
            output,
        }
    }
}
