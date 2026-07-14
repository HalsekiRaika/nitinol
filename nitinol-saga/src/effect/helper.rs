use std::time::Duration;

use nitinol_eventsource::{Aggregate, AggregateTellTarget, Decider};

use crate::effect::core::{SagaEffect, ScheduleSpec, TellIntent};
use crate::scheduler::TimerName;

impl<E> SagaEffect<E> {
    /// Returns the identity element of the Monoid — an effect that does nothing.
    pub fn empty() -> Self {
        Self::None
    }

    /// Returns the single-responsibility termination marker (D-13).
    ///
    /// Interpretation stops the saga process and tears down its upstream
    /// subscription.  Effects placed after `End` inside a `Sequence` are not
    /// interpreted.
    pub fn end() -> Self {
        Self::End
    }

    /// Persist a single event to the saga's event store and apply it to the
    /// saga state.
    pub fn persist(event: E) -> Self {
        Self::Persist {
            events: vec![event],
            tells: Vec::new(),
            schedules: Vec::new(),
        }
    }

    /// Persist multiple events to the saga's event store and apply them in
    /// order.
    ///
    /// An empty vector still produces a `Persist` variant — semantically
    /// distinct from `None` so that "intent to persist zero events" remains
    /// visible to the interpreter.
    pub fn persist_all(events: Vec<E>) -> Self {
        Self::Persist {
            events,
            tells: Vec::new(),
            schedules: Vec::new(),
        }
    }

    /// Send a typed command to a target aggregate.
    ///
    /// Builds a `Persist { events: [], tells: [intent], schedules: [] }` so
    /// the tell goes through the same Outbox-atomic path as any other tell.
    ///
    /// `C: Clone` is required because the retry executor re-`tell`s with a
    /// cloned command on every attempt.
    ///
    /// `C: serde::Serialize` is required so the command can be serialized
    /// into the `TellRequested` crash-restart payload.  When the saga process
    /// restarts after a full OS-process crash, registering a
    /// [`crate::SagaProps::with_crash_restart_factory`] allows the factory to
    /// receive the serialized bytes and reconstruct the [`TellIntent`] for
    /// re-dispatch.
    ///
    /// # Panics
    ///
    /// Panics if `serde_json::to_vec(&cmd)` fails.  This should not happen for
    /// well-formed `Serialize` implementations; it would indicate a bug in the
    /// command's serialization logic.
    pub fn tell<A, C, T>(target: T, cmd: C) -> Self
    where
        A: Aggregate + Decider<C>,
        C: Clone + serde::Serialize + Send + Sync + 'static,
        T: AggregateTellTarget<A>,
    {
        let crash_restart_payload = serde_json::to_vec(&cmd).map(bytes::Bytes::from).expect(
            "SagaEffect::tell: command serialization failed; \
                     ensure the command type implements serde::Serialize correctly",
        );
        Self::Persist {
            events: Vec::new(),
            tells: vec![TellIntent::new_with_crash_restart::<A, C, T>(
                target,
                cmd,
                crash_restart_payload,
            )],
            schedules: Vec::new(),
        }
    }

    /// Set (not merge) the list of `TellIntent`s attached to a `Persist`
    /// branch.  Calling it twice keeps only the final list.
    ///
    /// # Panics
    ///
    /// Panics if `self` is not a `Persist` variant.  Calling `with_tells` on
    /// `None` / `End` / `Sequence` is a Builder contract violation; silent
    /// identity / silent Persist-wrapping would obscure the misuse.
    pub fn with_tells(self, tells: Vec<TellIntent>) -> Self {
        match self {
            Self::Persist {
                events, schedules, ..
            } => Self::Persist {
                events,
                tells,
                schedules,
            },
            _ => panic!(
                "SagaEffect::with_tells may only be called on a Persist branch; \
                 call SagaEffect::persist(...) or SagaEffect::persist_all(...) first"
            ),
        }
    }

    /// Set (not merge) the list of [`ScheduleSpec`]s attached to a `Persist`
    /// branch.  Calling it twice keeps only the final list.
    ///
    /// # Panics
    ///
    /// Panics if `self` is not a `Persist` variant.  Calling `with_schedules`
    /// on `None` / `End` / `Sequence` / `CancelSchedule` is a Builder contract
    /// violation.
    pub fn with_schedules(self, schedules: Vec<ScheduleSpec>) -> Self {
        match self {
            Self::Persist { events, tells, .. } => Self::Persist {
                events,
                tells,
                schedules,
            },
            _ => panic!(
                "SagaEffect::with_schedules may only be called on a Persist branch; \
                 call SagaEffect::persist(...) or SagaEffect::persist_all(...) first"
            ),
        }
    }

    /// Schedule a typed message to be delivered to [`crate::Saga::on_scheduled`]
    /// after `after` has elapsed, keyed by `name` (E-29).
    ///
    /// Builds a `Persist { events: [], tells: [], schedules: [spec] }` whose
    /// spec carries the `serde_json`-serialized message as its payload.
    /// Re-using `name` supersedes the earlier schedule.
    ///
    /// # Panics
    ///
    /// Panics if `serde_json::to_vec(&message)` fails — this indicates a bug in
    /// the message type's `Serialize` implementation.
    pub fn schedule<M>(name: TimerName, after: Duration, message: M) -> Self
    where
        M: serde::Serialize,
    {
        let payload = serde_json::to_vec(&message).map(bytes::Bytes::from).expect(
            "SagaEffect::schedule: message serialization failed; \
             ensure the scheduled message type implements serde::Serialize correctly",
        );
        Self::Persist {
            events: Vec::new(),
            tells: Vec::new(),
            schedules: vec![ScheduleSpec {
                name,
                after,
                payload,
            }],
        }
    }

    /// Cancel the pending timer registered under `name` for this saga (E-28 /
    /// E-29).
    pub fn cancel_schedule(name: TimerName) -> Self {
        Self::CancelSchedule(name)
    }

    /// Append `End` after `self` via the Monoid `combine`, preserving order.
    ///
    /// `None.then_end()` collapses to `End` by the Monoid identity rule
    /// (`None.combine(End) == End`).
    pub fn then_end(self) -> Self {
        self.combine(Self::End)
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
