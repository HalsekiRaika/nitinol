use std::time::Duration;

use nitinol_eventsource::{Aggregate, AggregateTellTarget, Decider};

use crate::effect::core::{SagaEffect, ScheduleSpec, TellIntent};
use crate::scheduler::TimerName;

impl<E> SagaEffect<E> {
    /// Returns the identity element of the Monoid — an effect that does nothing.
    pub fn empty() -> Self {
        Self::None
    }

    /// Returns the single-responsibility termination marker.
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
    /// `None` / `End` / `Sequence` / `CancelSchedule` is a Builder contract
    /// violation; silent identity / silent Persist-wrapping would obscure the
    /// misuse.  Note that chaining two `Persist` branches through
    /// [`SagaEffect::combine`] yields a `Persist`, not a `Sequence`, so it stays
    /// a valid receiver.
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
    /// violation.  Note that chaining two `Persist` branches through
    /// [`SagaEffect::combine`] yields a `Persist`, not a `Sequence`, so it stays
    /// a valid receiver.
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
    /// after `after` has elapsed, keyed by `name`.
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

    /// Cancel the pending timer registered under `name` for this saga.
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
    /// When the two effects meeting at the junction are both `Persist`
    /// branches, they are concatenated into one `Persist` so the interpreter
    /// writes them as a single atomic `store::append` batch.  Without this,
    /// `persist(e).combine(tell(..))` would split the domain event and its
    /// `TellRequested` outbox marker across two appends, and a crash in between
    /// would drop the marker and lose the tell.  Only the junction pair merges:
    /// a `Persist` sitting behind an `End` or a `CancelSchedule` is never
    /// pulled across that leaf.  `None` is not such a leaf — as the identity it
    /// carries no interpretation of its own, so a `None` left on the junction by
    /// a hand-built `Sequence` is dropped rather than blocking the merge.
    ///
    /// Each of `events`, `tells` and `schedules` keeps its own left-to-right
    /// order, but the three categories do not interleave: a merged branch is
    /// always written as all events, then all tells, then all schedules — the
    /// fixed layout every `Persist` batch already has.
    ///
    /// Identity law:  `None.combine(a) == a` and `a.combine(None) == a`.
    /// Associativity: `(a.combine(b)).combine(c) == a.combine(b.combine(c))`.
    pub fn combine(self, other: Self) -> Self {
        match (self, other) {
            (Self::None, e) | (e, Self::None) => e,
            (Self::Sequence(mut a), Self::Sequence(mut b)) => {
                Self::drop_leading_identity(&mut b);
                let mut tail = b.into_iter();
                if let Some(head) = tail.next() {
                    Self::push_merging(&mut a, head);
                    a.extend(tail);
                }
                Self::Sequence(a)
            }
            (Self::Sequence(mut a), b) => {
                Self::push_merging(&mut a, b);
                Self::Sequence(a)
            }
            (a, Self::Sequence(mut b)) => {
                Self::prepend_merging(&mut b, a);
                Self::Sequence(b)
            }
            (mut a, b) => match Self::merge_into(&mut a, b) {
                None => a,
                Some(b) => Self::Sequence(vec![a, b]),
            },
        }
    }

    /// Concatenate `right` into `left` — events, tells and schedules each
    /// joined left to right — when both are `Persist` branches, so the pair
    /// reaches the store as one atomic batch.
    ///
    /// Returns `right` untouched when the junction is not `Persist × Persist`.
    fn merge_into(left: &mut Self, right: Self) -> Option<Self> {
        match (left, right) {
            (
                Self::Persist {
                    events,
                    tells,
                    schedules,
                },
                Self::Persist {
                    events: right_events,
                    tells: right_tells,
                    schedules: right_schedules,
                },
            ) => {
                events.extend(right_events);
                tells.extend(right_tells);
                schedules.extend(right_schedules);
                None
            }
            (_, right) => Some(right),
        }
    }

    /// Append `effect` to `seq`, folding it into the trailing element when both
    /// are `Persist`.
    ///
    /// `None` is the Monoid identity, so it never reaches `seq` and a trailing
    /// `None` carried by a hand-built `Sequence` is discarded instead of being
    /// taken as the junction.  An identity element that split an atomic batch
    /// would be observable in the store, which is exactly what an identity must
    /// never be.  `End` and `CancelSchedule` do carry their own interpretation
    /// and stay hard boundaries.
    fn push_merging(seq: &mut Vec<Self>, effect: Self) {
        if matches!(effect, Self::None) {
            return;
        }
        while matches!(seq.last(), Some(Self::None)) {
            seq.pop();
        }
        let unmerged = match seq.last_mut() {
            Some(last) => Self::merge_into(last, effect),
            None => Some(effect),
        };
        if let Some(effect) = unmerged {
            seq.push(effect);
        }
    }

    /// Prepend `effect` to `seq`, folding the leading element into it when both
    /// are `Persist`.
    ///
    /// `effect` leads the junction, so it is the left-hand side of the
    /// concatenation and the absorbed head leaves the vector.
    ///
    /// A leading `None` is dropped before the junction is taken, mirroring
    /// [`Self::push_merging`]: the identity must not decide whether the two
    /// `Persist` branches around it reach the store as one batch.
    fn prepend_merging(seq: &mut Vec<Self>, mut effect: Self) {
        Self::drop_leading_identity(seq);
        if seq.is_empty() {
            seq.push(effect);
            return;
        }
        let head = seq.remove(0);
        if let Some(head) = Self::merge_into(&mut effect, head) {
            seq.insert(0, head);
        }
        seq.insert(0, effect);
    }

    /// Drop the identity elements in front of `seq` so that the junction is
    /// taken against the first leaf that carries an interpretation of its own.
    ///
    /// Without this, a `None` at the head of a hand-built `Sequence` would
    /// decide whether the `Persist` branches on either side of the junction
    /// reach the store as one atomic batch — an observable identity, which the
    /// identity law forbids.  `End` and `CancelSchedule` do carry their own
    /// interpretation and are therefore left in place as real boundaries.
    fn drop_leading_identity(seq: &mut Vec<Self>) {
        while matches!(seq.first(), Some(Self::None)) {
            seq.remove(0);
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

    /// Recursively hoist nested `Sequence` elements to the parent level and
    /// fold every adjacent `Persist × Persist` junction into one `Persist`,
    /// exactly as [`Self::combine`] would if the effect had been assembled
    /// through the builder API.
    ///
    /// All `SagaEffect` variants — including `Sequence` and `Persist` — are
    /// public, so a caller can hand-build `Sequence(vec![Persist(domain),
    /// Persist(tell)])` without ever calling `combine`. Left unnormalized,
    /// the interpreter would execute that as two separate `store::append`
    /// calls, splitting the atomic batch `combine`'s doc contract promises
    /// and reopening the tell-loss window a crash between the two appends
    /// creates. The interpreter therefore runs every effect through this
    /// method before interpreting a `Sequence`, so the atomic-batch guarantee
    /// holds no matter how the effect was constructed.
    ///
    /// `End` and `CancelSchedule` are hard boundaries — reusing
    /// [`Self::push_merging`] means only a `Persist` immediately following
    /// another `Persist` in the flattened list merges, mirroring `combine`'s
    /// junction rule (see [`Self::merge_into`]) exactly rather than
    /// approximating it.
    ///
    /// Neither an explicit `SagaEffect::None` element nor a *nested* `Sequence`
    /// is a boundary. `None` is the Monoid identity, so `push_merging` drops it;
    /// were it kept, `Sequence([Persist(e), None, Persist(tell)])` would append
    /// `e` and the `TellRequested` marker separately and the identity element
    /// would decide whether the tell survives a crash. A nested `Sequence` —
    /// including an empty one — is not a leaf either: its own zero, one or many
    /// elements are hoisted directly into the parent's flat list by
    /// [`Self::hoist_into`].
    pub(crate) fn normalize_for_interpretation(self) -> Self {
        match self {
            Self::Sequence(children) => {
                let mut flat: Vec<Self> = Vec::new();
                for child in children {
                    Self::hoist_into(child, &mut flat);
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

    /// Recursively hoist `effect` into `out`, flattening every nested
    /// `Sequence` — including empty ones, which contribute zero leaves — and
    /// folding each adjacent `Persist × Persist` junction via
    /// [`Self::push_merging`] as it lands.
    ///
    /// A nested `Sequence` contributes its own leaves rather than itself, and
    /// [`Self::push_merging`] drops the identity `None`, so neither shape can
    /// end up separating two `Persist` branches in `out`.
    fn hoist_into(effect: Self, out: &mut Vec<Self>) {
        match effect {
            Self::Sequence(children) => {
                for child in children {
                    Self::hoist_into(child, out);
                }
            }
            leaf => Self::push_merging(out, leaf),
        }
    }
}
