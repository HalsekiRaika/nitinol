use nitinol_runtime::process::{Process, ProcessProxy, Receive, Stream};
use nitinol_runtime::{BoxedMessage, Message};

use crate::effect::core::Effect;
use crate::effect::publish::TypedPublish;
use crate::effect::tell::TypedTell;

impl<E> Effect<E> {
    /// Returns the identity element of the Monoid — an effect that does nothing.
    pub fn empty() -> Self {
        Self::None
    }

    /// Persist a single event to the `EventStore` and apply it to the aggregate.
    pub fn persist(event: E) -> Self {
        Self::Persist(vec![event])
    }

    /// Persist multiple events to the `EventStore` and apply them to the aggregate.
    pub fn persist_all(events: Vec<E>) -> Self {
        Self::Persist(events)
    }

    /// Apply a single event to the aggregate without persisting it.
    pub fn apply_only(event: E) -> Self {
        Self::Apply(vec![event])
    }

    /// Send a typed message to a target process.
    ///
    /// The `P: Process + Receive<M>` constraint is checked at compile time,
    /// so passing a message of the wrong type is a compile error.
    pub fn tell<P, M>(target: ProcessProxy<P>, msg: M) -> Self
    where
        P: Process + Receive<M>,
        M: Send + Sync + 'static,
    {
        Self::Side(Box::new(TypedTell {
            target,
            message: msg,
        }))
    }

    /// Publish a typed message to a `Stream<BoxedMessage>`.
    pub fn publish<M: Message>(stream: ProcessProxy<Stream<BoxedMessage>>, msg: M) -> Self {
        Self::Side(Box::new(TypedPublish {
            stream,
            message: msg,
        }))
    }

    /// Associative binary operation of the Monoid.
    ///
    /// Combines `self` and `other` into a single `Effect` while keeping the
    /// internal `Sequence` flat — no nesting is introduced.
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
    /// - A `Sequence` of zero elements collapses to `None`.
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
