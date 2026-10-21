//! The official fan-out pattern: one fact event, one saga decision, a delivery
//! host, idempotent creation.
//!
//! nitinol has exactly one unit of atomicity — one `append` to one stream — and
//! deliberately offers no atomic append spanning several streams.  That rule is
//! stated at contract level on
//! [`nitinol_persistence::store::EventStore::append`]; the decision record for
//! *why* — prior art surveyed, and the conditions under which it could be
//! revisited — is the ADR at
//! <https://github.com/HalsekiRaika/nitinol/issues/74>.  Cross-stream
//! consistency is expressed with fact events plus at-least-once delivery and
//! idempotency.  This crate is that expression, written out, on a payroll run
//! that owes 32 employees a payslip.
//!
//! # The shape
//!
//! 1. **One fact event.**  [`payroll_run::PayrollRun`] records the whole
//!    approval as a single [`payroll_run::PayrollRunApproved`] on its own
//!    stream.  Several events in one stream are still one atomic write
//!    (`append(Vec)`), so a decision that fits in one stream never needs more
//!    than nitinol offers.
//! 2. **One saga decision.**  [`saga::FanOutSaga`], run under a
//!    `SagaManagerProps` manager, subscribes to that fact and persists a single
//!    [`saga::FanOutStarted`] naming every one of the 32 payslips the fan-out
//!    owes.  That append also commits the position of the fact it was derived
//!    from, so a redelivery of the fact — the subscription is at-least-once
//!    too — is dropped before [`nitinol_saga::Saga::handle`] and the fan-out is
//!    not decided twice.
//! 3. **A delivery host carries the commands.**
//!    [`nitinol_saga::Saga::dispatch`] is a pure function from that one decision
//!    to 32 [`nitinol_saga::SagaDelivery`] values, and the delivery host that
//!    [`nitinol_saga::SagaSystemExt::host`] starts beside the manager polls the
//!    saga's decisions, calls the mapping, and delivers each command with
//!    `ask`.  What that host guarantees is written down on
//!    [`nitinol_saga::SagaDeliveryProps`].
//! 4. **Idempotent creation.**  [`payslip::Payslip`] answers a repeated
//!    [`payslip::IssuePayslip`] with "already issued" instead of a second write.
//!    A creation-only fan-out therefore needs no compensation: the recovery for
//!    an interrupted fan-out is to carry its commands again.
//!
//! # Where the durability actually is
//!
//! It is worth being precise, because this is the part the pattern turns on.
//!
//! * The saga's own stream holds **two rows** for one fan-out: the decision, and
//!   the consumed position of the fact it was derived from, committed by the one
//!   append.  Nothing per-child.  Nothing about delivery.
//! * The delivery host's only state is a **offset**: the position of the last
//!   record whose deliveries all landed.  It is recorded *after* the deliveries,
//!   never before.
//! * Each payslip's own stream says whether that payslip exists.
//!
//! Three owners, three different facts, and no fact with two owners.  "How far
//! did the fan-out get?" is answered by querying the payslip streams, not by a
//! counter the saga keeps — a counter would be a second owner of the same fact,
//! and a crash between the two writes would leave them disagreeing with no way
//! to tell which is right.
//!
//! # Why a crash cannot lose part of the fan-out
//!
//! The decision is durable before anything is delivered, and the offset
//! moves only after a record's deliveries have landed.  Those two orderings are
//! the whole argument:
//!
//! * A crash before the decision lands leaves the fact unconsumed, so the
//!   subscription redelivers it and the fan-out is decided again from scratch.
//! * A crash after the decision lands leaves an offset still standing below
//!   it.  A restarted host reads the decision again, re-derives all 32 commands
//!   from it, and carries the remaining ones — together with some it already
//!   carried, which the payslips absorb with a write-free acceptance.
//!
//! What there is *not* is a third state: a fan-out half recorded as having been
//! dispatched.  Nothing of the dispatch is recorded, so there is nothing to be
//! half of.  The commands are re-derived from the decision on every read.
//!
//! The same orderings are why delivery is at-least-once with no mode to choose:
//! moving the offset first would lose a delivery to the same crash instead
//! of repeating one, and making the receiver's write and the offset one
//! transaction would need an atomic write across two stores.
//! [`nitinol_saga::SagaDeliveryProps`] is where that is written down in full.
//!
//! # A refused delivery is held, not skipped
//!
//! A command the host cannot get carried out stops the host: it leaves its
//! offset where it is, retries the same record, and reports through
//! `tracing`.  It does **not** move on to the rest of the fan-out.  That is
//! deliberate — a command silently dropped because its target refused it is a
//! decision the saga made and nobody carried out, with nothing left to say so —
//! and it is directly observable here: a fan-out whose ninth payslip cannot be
//! written stops at the ninth, rather than issuing the other 23 and leaving a
//! gap nobody will ever notice.
//! [`SagaDeliveryProxy::advance_offset`](nitinol_saga::SagaDeliveryProxy::advance_offset)
//! is the way past such a hold, and it is deliberately a person's to take.
//!
//! # One store, many streams
//!
//! [`nitinol_persistence::store::EventStore`] is keyed by stream —
//! `append(key, events)` and `load(LoadQuery)` both name the stream they act
//! on — so a single instance holds the payroll run's stream, the 32 payslip
//! streams and the saga's own journal side by side, each owner reading only its
//! own key.  This crate binds exactly one store to its `EventSourceSystem`, and
//! every wiring point takes it from there: the run aggregate, the payslips, the
//! saga journal, the manager's subscription, and the delivery host's input.
//!
//! The host needs one slot beyond the journal: somewhere to keep its offset.
//! That is the provider's *saga-offset* slot, and
//! [`nitinol_saga::SagaSystemExt`] — the trait that supplies
//! `system.host::<FanOutSaga>(..)` — exists only on a system whose store holds
//! one.  Hosting a saga is starting the host that carries out its decisions, so
//! a deployment that forgot to name a store for that progress is told so while
//! the wiring compiles, not when a host first tries to record where it got to.
//!
//! A host whose journal and offset store are named by hand rather than resolved
//! from the system's own supplier is [`nitinol_saga::SagaDeliveryProps`] — the
//! explicit form, for a deployment that delivers out of a supplier other than
//! the one its system was built around.  Nothing here names one: a second host
//! beside the first is this same `host` verb declared again, which is how the
//! tests carry one fan-out's commands a second time.
//!
//! # Routing without a router
//!
//! A fan-out addresses children that may not be running, so it needs "given a
//! stream key, name the aggregate that owns it".  That is what
//! [`nitinol_eventsource::AggregateProxy`] is — a reference to an aggregate by
//! identity, resolved when a command is dispatched rather than when the
//! reference is built — so this crate keeps no registry of live children.
//! Building one is exactly what
//! [`SagaDelivery::new`](nitinol_saga::SagaDelivery::new) does with the
//! `AggregateId` the mapping hands it, inside the host.
//!
//! Note where that leaves the saga: holding nothing.  It needs no reference to
//! anything, because it never sends anything — so [`saga::FanOutSaga`] is a unit
//! struct, and the producer the manager calls ignores the components it is
//! handed.
//!
//! A subscription does not need a store of its own either.  The manager is
//! handed one [`nitinol_saga::Subscription`] per source the saga's alphabet
//! declares, and a subscription is a store plus a stream key plus a start
//! position: the key filters the shared store down to the run's own stream, and
//! [`Subscription::with_after`](nitinol_saga::Subscription::with_after) names a
//! position to resume past.  Splitting streams across store instances is a
//! deployment decision — separate backends, separate retention — not something
//! the pattern requires, which is why `system.subscription(key)` takes the
//! store the system already holds.
//!
//! # Choosing the stream that owns the decision
//!
//! The owner is the stream whose invariant the decision belongs to — the one
//! that would be wrong if the decision were recorded twice or not at all.  Here
//! that is the payroll run: "this run was approved for these 32 employees" is a
//! fact about the run, and each payslip's existence is a consequence of it, not
//! part of it.  Two questions settle most cases:
//!
//! * *Which single stream must reject a contradictory second decision?*  That
//!   stream owns it.  If the answer is "several streams together", the boundary
//!   is drawn in the wrong place — an aggregate is the consistency boundary.
//! * *Can the consequence be re-derived from the decision alone?*  If yes, the
//!   consequence belongs downstream of a fact event, not inside the same write.
//!
//! The second question is also the test the saga's own decision has to pass, for
//! a stricter reason: [`nitinol_saga::Saga::dispatch`] is literally a pure
//! function of it.
//!
//! # Make the records self-sufficient
//!
//! Both hops are handed a decoded record and nothing else:
//!
//! * `Saga::correlate` is handed the trigger — not the stream key it was read
//!   from, not its sequence.  So
//!   [`payroll_run::PayrollRunApproved`] carries its own `payroll_run` key and
//!   the full employee list; together they are what
//!   [`payslip::Payslip::stream_key`] needs.
//! * `Saga::dispatch` is handed the saga's own decision — no `&self`, no system,
//!   no saga anywhere, since a host keeps none resident and may be reading a
//!   record written by an incarnation that is gone.  So
//!   [`saga::FanOutStarted`] lists all 32 payslip stream keys.
//!
//! An event that leaves out what its consumers need forces every consumer to
//! reach back to where it came from, and a consumer that cannot — a projection
//! replaying an archive, a second host on another subscription — simply cannot
//! act on it.
//!
//! The usual third thing a decision must carry is the operation id its receiver
//! recognises a repeat by.  This fan-out is the case where that degenerates:
//! a payslip is issued at most once in its whole life, so its own existence
//! *is* the repeat marker, and [`payslip::IssuePayslip`] carries no payload at
//! all.  `examples/saga/3.idempotent-receive` is the general case.
//!
//! # Two further steps, not implemented here
//!
//! * **Genesis by reference.**  If a payslip's initial state is derivable from
//!   the fact event, the payslip stream need not be written at fan-out time at
//!   all: creation can be deferred to the first real command, with the fact
//!   event as the reference for what it started as.  That trades 32 writes for
//!   none, at the cost of a payslip that is not yet visible in the store.
//! * **Closing the books.**  If a reader needs "all 32 arrived" as a single
//!   fact, express it as one summary event written once the count is reached,
//!   rather than by making readers aggregate 32 streams themselves.
//!
//! # What the tests fix
//!
//! * `tests/redelivery_is_noop.rs` — one approval reaches the run's stream as
//!   one record, becomes one decision of two rows on the saga's own stream, and
//!   fans out to 32 payslips; a second delivery host reading the same decision
//!   from an offset of its own carries all 32 commands again and leaves every
//!   payslip stream byte-identical, without the saga's stream growing by a row;
//!   an issue replayed all the way down to the store conflicts on the genesis
//!   sequence while leaving the first write intact; and two runs covering one
//!   employee address separate payslip streams.
//! * `tests/crash_replay_completes_fanout.rs` — a fan-out whose host cannot
//!   reach the last 24 payslips stops at the first of them with its offset
//!   unmoved, and a second host reading that same offset completes all 32
//!   without rewriting the 8 that already existed.

pub mod codec;
pub mod payroll_run;
pub mod payslip;
pub mod saga;
