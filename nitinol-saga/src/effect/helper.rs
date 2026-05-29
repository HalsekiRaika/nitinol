use std::marker::PhantomData;

use nitinol_eventsource::{Aggregate, AggregateTellTarget, Decider};

use crate::effect::core::{SagaEffect, SagaTellEffect};
use crate::effect::tell::TypedSagaTell;

impl<E> SagaEffect<E> {
    /// Returns the identity element of the Monoid — an effect that does nothing.
    pub fn empty() -> Self {
        Self::None
    }

    /// Persist a single event to the saga's event store and apply it to the
    /// saga state.
    pub fn persist(event: E) -> Self {
        Self::Persist(vec![event])
    }

    /// Persist multiple events to the saga's event store and apply them in
    /// order.
    ///
    /// An empty vector still produces a `Persist` variant — semantically
    /// distinct from `None` so that "intent to persist zero events" remains
    /// visible to the interpreter.
    pub fn persist_all(events: Vec<E>) -> Self {
        Self::Persist(events)
    }

    /// Send a typed command to a target aggregate.
    ///
    /// The `A: Decider<C>` constraint is checked at compile time, so passing
    /// a command of the wrong type is a compile error.  Execution is
    /// fire-and-forget: if the send fails, the error is logged and the saga
    /// continues (consistent with `Effect::Side` semantics in
    /// `nitinol-eventsource`).
    pub fn tell<A, C, T>(target: T, cmd: C) -> Self
    where
        A: Aggregate + Decider<C>,
        C: Send + Sync + 'static,
        T: AggregateTellTarget<A>,
    {
        Self::Tell(SagaTellEffect(Box::new(TypedSagaTell {
            target,
            cmd,
            _phantom: PhantomData::<fn() -> A>,
        })))
    }

    /// Associative binary operation of the Monoid.
    ///
    /// Combines `self` and `other` into a single `SagaEffect` while keeping
    /// the internal `Sequence` flat — no nesting is introduced.
    ///
    /// Identity law:  `None.combine(a) == a` and `a.combine(None) == a`.
    /// Associativity: `(a.combine(b)).combine(c) == a.combine(b.combine(c))`.
    pub fn combine(self, other: Self) -> Self {
        match (self, other) {
            (Self::None, e) | (e, Self::None) => e,
            (Self::Sequence(mut a), Self::Sequence(b)) => {
                a.extend(b);
                Self::Sequence(a)
            }
            (Self::Sequence(mut a), b) => {
                a.push(b);
                Self::Sequence(a)
            }
            (a, Self::Sequence(mut b)) => {
                b.insert(0, a);
                Self::Sequence(b)
            }
            (a, b) => Self::Sequence(vec![a, b]),
        }
    }

    /// Recursively flatten nested `Sequence` variants into a single level.
    ///
    /// - A `Sequence` of zero non-`None` elements collapses to `None`.
    /// - A `Sequence` of one element is unwrapped to that element.
    /// - All other variants are returned unchanged.
    pub fn flatten(self) -> Self {
        match self {
            Self::Sequence(children) => {
                let mut flat: Vec<Self> = Vec::new();
                for child in children {
                    match child.flatten() {
                        Self::Sequence(inner) => flat.extend(inner),
                        Self::None => {}
                        other => flat.push(other),
                    }
                }
                match flat.len() {
                    0 => Self::None,
                    1 => flat.pop().expect("len was 1"),
                    _ => Self::Sequence(flat),
                }
            }
            other => other,
        }
    }
}
