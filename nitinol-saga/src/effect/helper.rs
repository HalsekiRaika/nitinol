use crate::effect::core::SagaEffect;

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
            schedules: Vec::new(),
        }
    }

    /// Persist multiple events to the saga's event store and apply them in
    /// order.
    ///
    /// An empty vector still produces a `Persist` variant, so the value remains
    /// a receiver that [`SagaEffect::combine`] folds schedules into.
    /// The interpreter, however, does not see an "intent to persist zero
    /// events": a `Persist` whose `events` **and** `schedules` are both
    /// empty takes the same no-op path as [`SagaEffect::None`] and persists no
    /// batch of its own — though the interpreter still writes a
    /// consumed-position-only append when the input this was decided from has
    /// a position not yet durable, so a no-op decision is not redelivered.
    ///
    /// Emptiness of `events` alone is not enough to make a branch a no-op — a
    /// `Persist` carrying a schedule but no user event is exactly what
    /// [`Deadline::set`](crate::Deadline::set) builds, and it is always
    /// interpreted.
    pub fn persist_all(events: Vec<E>) -> Self {
        Self::Persist {
            events,
            schedules: Vec::new(),
        }
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
    /// `persist(e).combine(deadline.set(..))` would split the domain event and
    /// the schedule fact across two appends, and a crash in between would arm a
    /// timer against a decision no store holds.  Only the junction pair merges:
    /// a `Persist` sitting behind an `End` or a `CancelSchedule` is never
    /// pulled across that leaf.  `None` is not such a leaf — as the identity it
    /// carries no interpretation of its own, so a `None` left on the junction by
    /// a hand-built `Sequence` is dropped rather than blocking the merge.
    ///
    /// Each of `events` and `schedules` keeps its own left-to-right
    /// order, but the two categories do not interleave: a merged branch is
    /// always written as all events, then all schedules — the fixed layout every
    /// `Persist` batch already has.
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

    /// Concatenate `right` into `left` — events and schedules each joined left
    /// to right — when both are `Persist` branches, so the pair reaches the
    /// store as one atomic batch.
    ///
    /// Returns `right` untouched when the junction is not `Persist × Persist`.
    fn merge_into(left: &mut Self, right: Self) -> Option<Self> {
        match (left, right) {
            (
                Self::Persist { events, schedules },
                Self::Persist {
                    events: right_events,
                    schedules: right_schedules,
                },
            ) => {
                events.extend(right_events);
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
    /// Persist(schedule)])` without ever calling `combine`. Left unnormalized,
    /// the interpreter would execute that as two separate `store::append`
    /// calls, splitting the atomic batch `combine`'s doc contract promises
    /// and reopening the window a crash between the two appends creates. The
    /// interpreter therefore runs every effect through this method before
    /// interpreting a `Sequence`, so the atomic-batch guarantee holds no matter
    /// how the effect was constructed.
    ///
    /// `End` and `CancelSchedule` are hard boundaries — reusing
    /// [`Self::push_merging`] means only a `Persist` immediately following
    /// another `Persist` in the flattened list merges, mirroring `combine`'s
    /// junction rule (see [`Self::merge_into`]) exactly rather than
    /// approximating it.
    ///
    /// Neither an explicit `SagaEffect::None` element nor a *nested* `Sequence`
    /// is a boundary. `None` is the Monoid identity, so `push_merging` drops it;
    /// were it kept, `Sequence([Persist(e), None, Persist(schedule)])` would
    /// append `e` and the schedule fact separately and the identity element
    /// would decide whether the timer is armed against a durable decision. A
    /// nested `Sequence` — including an empty one — is not a leaf either: its
    /// own zero, one or many elements are hoisted directly into the parent's
    /// flat list by [`Self::hoist_into`].
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

#[cfg(test)]
mod tests {
    //! The schedule category of the effect algebra.
    //!
    //! `combine`'s treatment of events is observable from outside the crate and
    //! is pinned there (`tests/effect_construction.rs`,
    //! `tests/effect_monoid.rs`, `tests/effect_attach_is_variant_agnostic.rs`).
    //! A [`ScheduleSpec`] is not: the only way to build one is
    //! [`Deadline::set`](crate::Deadline::set), and a
    //! [`Deadline`](crate::Deadline) only exists inside the producer the spawn
    //! builder calls — which is the point, because "arm a timer without writing
    //! the fact" must not be expressible.  So the schedule-carrying cases of the
    //! same algebra live here, where a spec can be named.

    use std::time::Duration;

    use bytes::Bytes;

    use crate::effect::core::ScheduleSpec;
    use crate::scheduler::TimerName;

    use super::*;

    fn spec(name: &str, secs: u64) -> ScheduleSpec {
        ScheduleSpec::new(
            TimerName::new(name),
            Duration::from_secs(secs),
            "test-message",
            "test-codec",
            Bytes::from_static(b"payload"),
        )
    }

    /// Borrow the `Persist` branch of an effect, failing loudly otherwise.
    fn persist_parts<E>(effect: &SagaEffect<E>) -> (&Vec<E>, &Vec<ScheduleSpec>) {
        match effect {
            SagaEffect::Persist { events, schedules } => (events, schedules),
            _ => panic!("expected a single `Persist` branch"),
        }
    }

    /// A schedule attached to the identity yields the schedule branch alone —
    /// `None` adds nothing and takes nothing away.
    #[test]
    fn a_schedule_combined_onto_none_yields_the_schedule_branch_alone() {
        let s = spec("x", 1);

        let effect = SagaEffect::<u32>::empty().combine(SagaEffect::Persist {
            events: Vec::new(),
            schedules: vec![s.clone()],
        });

        let (events, schedules) = persist_parts(&effect);
        assert!(events.is_empty(), "the identity must add no event");
        assert_eq!(
            schedules,
            &vec![s],
            "None is the Monoid identity, so attaching a schedule to it must yield \
             the schedule branch unchanged"
        );
    }

    /// A decision and the deadline it sets fold into ONE `Persist`, which is
    /// what makes the interpreter write them as a single atomic batch — a timer
    /// can never be armed against a decision no store holds.
    #[test]
    fn a_decision_combined_with_a_schedule_yields_one_persist() {
        let s = spec("deadline", 60);

        let effect = SagaEffect::persist(13u32).combine(SagaEffect::Persist {
            events: Vec::new(),
            schedules: vec![s.clone()],
        });

        let (events, schedules) = persist_parts(&effect);
        assert_eq!(events, &vec![13u32]);
        assert_eq!(
            schedules,
            &vec![s],
            "the event and its scheduled marker must share a single `Persist`, and \
             therefore a single atomic append batch"
        );
    }

    /// Two schedules concatenate in the order they were combined: a saga setting
    /// two deadlines in one decision loses neither, and their order is its own.
    #[test]
    fn two_schedules_concatenate_in_left_to_right_order() {
        let first = spec("a", 30);
        let second = spec("b", 60);

        let effect = SagaEffect::persist(2u32)
            .combine(SagaEffect::Persist {
                events: Vec::new(),
                schedules: vec![first.clone()],
            })
            .combine(SagaEffect::Persist {
                events: Vec::new(),
                schedules: vec![second.clone()],
            });

        let (events, schedules) = persist_parts(&effect);
        assert_eq!(events, &vec![2u32]);
        assert_eq!(
            schedules,
            &vec![first, second],
            "chained schedule attachments must keep both specs in left-to-right order"
        );
    }

    /// A pair of `Persist` branches already folded into one still accepts a
    /// schedule without splitting the batch.
    #[test]
    fn a_merged_persist_pair_combined_with_a_schedule_stays_one_persist() {
        let s = spec("after-merge", 3);

        let merged = SagaEffect::persist(1u32).combine(SagaEffect::persist(2u32));
        let effect = merged.combine(SagaEffect::Persist {
            events: Vec::new(),
            schedules: vec![s.clone()],
        });

        let (events, schedules) = persist_parts(&effect);
        assert_eq!(events, &vec![1u32, 2]);
        assert_eq!(
            schedules,
            &vec![s],
            "folding the pair into a `Sequence` instead would split the scheduled \
             marker out of the events' append batch"
        );
    }

    /// `End` is a hard boundary: a schedule behind it stays its own leaf rather
    /// than being pulled into the terminated batch.
    #[test]
    fn a_schedule_combined_after_end_stays_behind_the_terminator() {
        let effect = SagaEffect::<u32>::end().combine(SagaEffect::Persist {
            events: Vec::new(),
            schedules: vec![spec("x", 1)],
        });

        match effect {
            SagaEffect::Sequence(children) => {
                assert_eq!(children.len(), 2, "combine must not nest or drop leaves");
                assert!(matches!(children[0], SagaEffect::End));
                assert!(matches!(children[1], SagaEffect::Persist { .. }));
            }
            _ => panic!("a schedule behind `End` must stay a leaf of its own"),
        }
    }

    /// Cancelling a deadline and setting a new one is a legitimate decision, and
    /// the cancellation is never absorbed into the schedule's branch.
    #[test]
    fn a_schedule_combined_after_a_cancellation_stays_its_own_leaf() {
        let cancel: SagaEffect<u32> = SagaEffect::CancelSchedule(TimerName::new("drop"));

        let effect = cancel.combine(SagaEffect::Persist {
            events: Vec::new(),
            schedules: vec![spec("x", 1)],
        });

        match effect {
            SagaEffect::Sequence(children) => {
                assert_eq!(children.len(), 2);
                assert!(matches!(children[0], SagaEffect::CancelSchedule(_)));
                assert!(matches!(children[1], SagaEffect::Persist { .. }));
            }
            _ => panic!("a cancellation followed by a new schedule must compose as two leaves"),
        }
    }

    /// Attaching a schedule to a `Sequence` appends it to the flat leaf list
    /// without nesting.
    #[test]
    fn a_schedule_combined_onto_a_sequence_appends_flatly() {
        // `Persist.combine(Persist)` folds into one `Persist`, so the Sequence
        // premise is built from leaves that never merge.
        let sequence = SagaEffect::persist(1u32).combine(SagaEffect::end());

        let effect = sequence.combine(SagaEffect::Persist {
            events: Vec::new(),
            schedules: vec![spec("x", 1)],
        });

        match effect {
            SagaEffect::Sequence(children) => {
                assert_eq!(children.len(), 3, "the leaf list must stay flat");
                assert!(matches!(children[0], SagaEffect::Persist { .. }));
                assert!(matches!(children[1], SagaEffect::End));
                assert!(matches!(children[2], SagaEffect::Persist { .. }));
            }
            _ => panic!("attaching a schedule to a `Sequence` must append a leaf"),
        }
    }

    /// The merge concatenates both categories at once, each in its own
    /// left-to-right order: dropping or reordering either loses saga intent.
    #[test]
    fn the_merge_concatenates_events_and_schedules_left_to_right() {
        let left_spec = spec("left", 1);
        let right_spec = spec("right", 2);

        let left = SagaEffect::persist_all(vec![1u32, 2]).combine(SagaEffect::Persist {
            events: Vec::new(),
            schedules: vec![left_spec.clone()],
        });
        let right = SagaEffect::persist(3u32).combine(SagaEffect::Persist {
            events: Vec::new(),
            schedules: vec![right_spec.clone()],
        });

        let merged = left.combine(right);

        let (events, schedules) = persist_parts(&merged);
        assert_eq!(events, &vec![1u32, 2, 3]);
        assert_eq!(
            schedules,
            &vec![left_spec, right_spec],
            "the merge must concatenate events AND schedules — each in \
             left-to-right order"
        );
    }
}
