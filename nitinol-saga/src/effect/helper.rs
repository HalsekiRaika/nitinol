use nitinol_eventsource::{Aggregate, AggregateTellTarget, Decider};

use crate::effect::core::{SagaEffect, TellIntent};

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
    /// An empty vector still produces a `Persist` variant, so the value remains
    /// a receiver that [`SagaEffect::combine`] folds tells and schedules into.
    /// The interpreter, however, does not see an "intent to persist zero
    /// events": a `Persist` whose `events`, `tells` **and** `schedules` are all
    /// empty takes the same no-op path as [`SagaEffect::None`] and persists no
    /// batch of its own — though the interpreter still writes a
    /// consumed-position-only append when the input this was decided from has
    /// a position not yet durable, so a no-op decision is not redelivered.
    ///
    /// Emptiness of `events` alone is not enough to make a branch a no-op — a
    /// `Persist` carrying a tell or a schedule but no user event is exactly what
    /// [`SagaEffect::tell`] and [`Deadline::set`](crate::Deadline::set) build,
    /// and it is always interpreted.
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
    /// # What the outbox delivers
    ///
    /// What follows is the delivery contract for every tell this crate
    /// dispatches.  It is stated here, once; anything else that needs it links
    /// here rather than restating it.
    ///
    /// A tell is **not** simply "at-least-once", and it is never exactly-once.
    /// What holds depends on which stage is being talked about and on how the
    /// saga was wired, so the stages are named separately:
    ///
    /// | Stage | What it means | What the outbox promises |
    /// |---|---|---|
    /// | Intent recorded | the `TellRequested` marker is committed | atomically with the saga events of the same `Persist` batch |
    /// | Dispatch attempted | [`AggregateProxy::tell`](nitinol_eventsource::AggregateProxy::tell) was called | up to a finite retry budget, see below |
    /// | Accepted for delivery | that call answered `Ok` | recorded as `TellAcked` on the saga's own stream |
    /// | Committed by the receiver | the target aggregate persisted its events | **nothing** — no marker observes this |
    /// | Business completion | the operation it asked for is done | **nothing** — see *Beyond acceptance* |
    ///
    /// The third and fourth rows are the ones that surprise.  `tell` answers
    /// when the command is *queued* on the target's mailbox, not when the target
    /// decided or committed it — [`AggregateProxy::tell`](nitinol_eventsource::AggregateProxy::tell)
    /// says so on its own surface — and the executor treats that answer as the
    /// tell settling and has `TellAcked` appended.
    ///
    /// ## Re-dispatch on replay
    ///
    /// A `TellRequested` with no `TellAcked` and no `TellFailed` after it is
    /// *pending*, and replay decides its fate in this order:
    ///
    /// 0. A saga whose stream carries a durable `Ended` marker skips this
    ///    entirely: a terminated saga re-dispatches nothing, ever.
    /// 1. **The in-memory intent is still held** — a supervised restart within
    ///    the same OS process, including one after a failed terminal-marker
    ///    append.  Re-dispatched.
    /// 2. **A crash-restart factory is wired, the marker carries a crash-restart
    ///    payload, and the factory answers `Some`.**  Re-dispatched.  All three
    ///    are required: an unwired factory, a marker written without a payload,
    ///    and a factory answering `None` all fall through to the next case.
    /// 3. **Otherwise** replay *attempts* to append a synthesized `TellFailed`
    ///    for it — an explicit, recorded give-up rather than a silent drop.  The
    ///    attempt can itself fail: the store may refuse the append, in which case
    ///    the claimed sequence is released and the marker stays pending for a
    ///    later incarnation to evaluate again.  When the append succeeds, a dead
    ///    letter follows, and a *failed* dead-letter append stops the process
    ///    before it subscribes, so the failure is not left half-recorded.
    ///
    /// So a saga with no crash-restart factory does not lose pending tells
    /// quietly — it gives up on them in writing.  Wiring the factory is what
    /// turns case 3 into case 2, and it is what makes
    /// [`SagaManagerProps::with_instance_passivation`](crate::SagaManagerProps::with_instance_passivation)
    /// lossless for in-flight tells as well.
    ///
    /// ## The retry budget is finite
    ///
    /// A dispatch is retried three times in total (one attempt plus two
    /// retries), with exponential backoff starting at 100 ms.  Exhausting it
    /// settles the tell as `TellFailed`, which reaches
    /// [`Saga::on_tell_failed`](crate::Saga::on_tell_failed).  "At-least-once"
    /// here therefore never means "retried until it arrives".
    ///
    /// ## Windows no wiring closes
    ///
    /// * Once `TellAcked` is persisted on the sending saga's stream, the tell is
    ///   settled.  If the receiving aggregate then lost an append race, or its
    ///   node died before it committed, nothing re-dispatches it — the factory
    ///   and the payload make no difference, because the marker is no longer
    ///   pending.
    /// * The converse is worth stating precisely, because it is the same
    ///   scenario one moment earlier: if the mailbox accepted the command but
    ///   the *sender* stopped before `TellAcked` reached its stream, the marker
    ///   is still pending, so the cases above apply and the command may be
    ///   delivered a second time.
    ///
    /// ## What re-dispatch reproduces, and what it does not
    ///
    /// The crash-restart payload is stored verbatim and handed to the factory
    /// verbatim, so a command re-dispatched from a marker is rebuilt from the
    /// same bytes that were dispatched the first time.  That is the whole of the
    /// framework's promise.  That the factory rebuilds *the same command
    /// addressed to the same target* from those bytes is the factory's
    /// obligation — nothing checks what it returns — and so is keeping the
    /// command's serialized shape and the factory itself compatible across a
    /// deployment that restarts with markers still pending.
    ///
    /// Note also that a re-dispatched marker is only one of the two ways the
    /// same operation is asked for twice.  The other is the same upstream event
    /// reaching [`Saga::handle`](crate::Saga::handle) again — a second
    /// incarnation catching up, or another node holding a duplicate activation —
    /// which builds a *new* marker around a freshly constructed command.  No
    /// bytes are re-decoded there, so an identifier minted inside `handle` comes
    /// out different every time.
    ///
    /// ## Beyond acceptance
    ///
    /// Nothing above promises the operation was carried out, and no envelope is
    /// attached to a dispatched command for a receiver to recognise a repeat by:
    /// the record an append is handed, the record a load returns and a decider's
    /// [`Decision`](nitinol_eventsource::Decision) carry the domain's own bytes
    /// and nothing beside them.  A receiver that must not carry an operation out
    /// twice is therefore the one that recognises the repeat, from data the
    /// domain put on the command.  That is the idempotent-receive pattern, worked
    /// through end to end — including the requirement that the operation id be
    /// derived so it survives the *second* duplication route above — in
    /// [the idempotent-receive example](https://github.com/HalsekiRaika/nitinol/tree/master/examples/saga/3.idempotent-receive).
    ///
    /// Promising *business completion* is a further step this crate does not
    /// take: it needs an acknowledgement the receiver emits after it commits, or
    /// a result event plus re-driving of operations still outstanding, and that
    /// is a mechanism of its own rather than a setting on this one.
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
        <A as Decider<C>>::Rejection: std::error::Error + Send + Sync + 'static,
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

    /// Put a pre-built [`TellIntent`] on a `Persist` branch of its own.
    ///
    /// Builds the same `Persist { events: [], tells: [intent], schedules: [] }`
    /// shape [`SagaEffect::tell`] does, so the tell takes the same Outbox-atomic
    /// path — but without the `serde_json` step, which is what makes an intent
    /// built through [`TellIntent::new`] (no crash-restart payload) expressible.
    ///
    /// Attach it to another effect with [`SagaEffect::combine`], which folds an
    /// adjacent `Persist × Persist` junction into one `Persist`: the tell's
    /// `TellRequested` marker then shares the events' atomic batch.  Combining
    /// several intents concatenates them — each attachment is kept.
    pub fn tell_intent(intent: TellIntent) -> Self {
        Self::Persist {
            events: Vec::new(),
            tells: vec![intent],
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

#[cfg(test)]
mod tests {
    //! The schedule category of the effect algebra.
    //!
    //! `combine`'s treatment of events and tells is observable from outside the
    //! crate and is pinned there (`tests/effect_construction.rs`,
    //! `tests/effect_monoid.rs`, `tests/effect_attach_is_variant_agnostic.rs`).
    //! A [`ScheduleSpec`] is not: the only way to build one is
    //! [`Deadline::set`](crate::Deadline::set), and a
    //! [`Deadline`](crate::Deadline) only exists inside the producer the spawn
    //! builder calls — which is the point, because "arm a timer without writing
    //! the fact" must not be expressible.  So the schedule-carrying cases of the
    //! same algebra live here, where a spec can be named.

    use std::time::Duration;

    use bytes::Bytes;
    use nitinol_eventsource::codec::Codec;
    use nitinol_eventsource::test_helpers::MockAggregateProxy;
    use nitinol_eventsource::{Decision, Event};
    use nitinol_persistence::{EventType, Family, TypeName};

    use crate::effect::core::ScheduleSpec;
    use crate::scheduler::TimerName;

    use super::*;

    fn spec(name: &str, secs: u64) -> ScheduleSpec {
        ScheduleSpec::new(
            TimerName::new(name),
            Duration::from_secs(secs),
            Bytes::from_static(b"payload"),
        )
    }

    /// Borrow the `Persist` branch of an effect, failing loudly otherwise.
    fn persist_parts<E>(effect: &SagaEffect<E>) -> (&Vec<E>, usize, &Vec<ScheduleSpec>) {
        match effect {
            SagaEffect::Persist {
                events,
                tells,
                schedules,
            } => (events, tells.len(), schedules),
            _ => panic!("expected a single `Persist` branch"),
        }
    }

    // A minimal aggregate, only so a `TellIntent` can be named alongside a
    // schedule: the three categories sharing one batch is the assertion, and a
    // tell is one of the three.

    #[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
    struct Acked;

    struct AckedCodec;

    impl Codec<Acked> for AckedCodec {
        type Error = std::convert::Infallible;

        fn encode(_event: &Acked) -> Result<Bytes, Self::Error> {
            Ok(Bytes::new())
        }

        fn decode(_payload: &[u8]) -> Result<Acked, Self::Error> {
            Ok(Acked)
        }
    }

    impl Event for Acked {
        const EVENT_TYPE: EventType =
            EventType::new(Family::new("effect_algebra"), TypeName::new("Acked"));
        type Codec = AckedCodec;
    }

    #[derive(Default)]
    struct Target;

    #[derive(Clone)]
    struct Ack;

    impl Aggregate for Target {
        type Event = Acked;

        fn apply(&mut self, _event: Acked) {}
    }

    impl Decider<Ack> for Target {
        type Output = ();
        type Rejection = std::convert::Infallible;

        fn decide(&self, _cmd: Ack) -> Decision<Acked, (), Self::Rejection> {
            Decision::persist(Vec::new()).output(())
        }
    }

    fn intent() -> TellIntent {
        TellIntent::new::<Target, Ack, _>(MockAggregateProxy::<Target>::new(), Ack)
    }

    /// A schedule attached to the identity yields the schedule branch alone —
    /// `None` adds nothing and takes nothing away.
    #[test]
    fn a_schedule_combined_onto_none_yields_the_schedule_branch_alone() {
        let s = spec("x", 1);

        let effect = SagaEffect::<u32>::empty().combine(SagaEffect::Persist {
            events: Vec::new(),
            tells: Vec::new(),
            schedules: vec![s.clone()],
        });

        let (events, tells, schedules) = persist_parts(&effect);
        assert!(events.is_empty(), "the identity must add no event");
        assert_eq!(tells, 0, "the identity must add no tell");
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
            tells: Vec::new(),
            schedules: vec![s.clone()],
        });

        let (events, tells, schedules) = persist_parts(&effect);
        assert_eq!(events, &vec![13u32]);
        assert_eq!(tells, 0);
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
                tells: Vec::new(),
                schedules: vec![first.clone()],
            })
            .combine(SagaEffect::Persist {
                events: Vec::new(),
                tells: Vec::new(),
                schedules: vec![second.clone()],
            });

        let (events, _, schedules) = persist_parts(&effect);
        assert_eq!(events, &vec![2u32]);
        assert_eq!(
            schedules,
            &vec![first, second],
            "chained schedule attachments must keep both specs in left-to-right order"
        );
    }

    /// An event, a tell and a schedule attached through `combine` land on ONE
    /// `Persist`: all three categories share the single atomic batch.
    #[test]
    fn an_event_a_tell_and_a_schedule_land_on_the_same_persist() {
        let s = spec("reminder", 5);

        let effect = SagaEffect::persist(99u32)
            .combine(SagaEffect::tell_intent(intent()))
            .combine(SagaEffect::Persist {
                events: Vec::new(),
                tells: Vec::new(),
                schedules: vec![s.clone()],
            });

        let (events, tells, schedules) = persist_parts(&effect);
        assert_eq!(events, &vec![99u32]);
        assert_eq!(
            tells, 1,
            "the tell must not be dropped by the schedule's merge"
        );
        assert_eq!(schedules, &vec![s]);
    }

    /// A pair of `Persist` branches already folded into one still accepts a
    /// schedule without splitting the batch.
    #[test]
    fn a_merged_persist_pair_combined_with_a_schedule_stays_one_persist() {
        let s = spec("after-merge", 3);

        let merged = SagaEffect::persist(1u32).combine(SagaEffect::persist(2u32));
        let effect = merged.combine(SagaEffect::Persist {
            events: Vec::new(),
            tells: Vec::new(),
            schedules: vec![s.clone()],
        });

        let (events, _, schedules) = persist_parts(&effect);
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
            tells: Vec::new(),
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
            tells: Vec::new(),
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
            tells: Vec::new(),
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

    /// The merge concatenates all three categories at once, each in its own
    /// left-to-right order: dropping or reordering any of them loses saga
    /// intent.
    #[test]
    fn the_merge_concatenates_events_tells_and_schedules_left_to_right() {
        let left_spec = spec("left", 1);
        let right_spec = spec("right", 2);

        let left = SagaEffect::persist_all(vec![1u32, 2])
            .combine(SagaEffect::tell_intent(intent()))
            .combine(SagaEffect::Persist {
                events: Vec::new(),
                tells: Vec::new(),
                schedules: vec![left_spec.clone()],
            });
        let right = SagaEffect::persist(3u32)
            .combine(SagaEffect::tell_intent(intent()))
            .combine(SagaEffect::Persist {
                events: Vec::new(),
                tells: Vec::new(),
                schedules: vec![right_spec.clone()],
            });

        let merged = left.combine(right);

        let (events, tells, schedules) = persist_parts(&merged);
        assert_eq!(events, &vec![1u32, 2, 3]);
        assert_eq!(tells, 2);
        assert_eq!(
            schedules,
            &vec![left_spec, right_spec],
            "the merge must concatenate events, tells AND schedules — each in \
             left-to-right order"
        );
    }
}
