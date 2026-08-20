//! The official fan-out pattern: one fact event, saga delivery, idempotent
//! creation.
//!
//! nitinol has exactly one unit of atomicity — one `append` to one stream — and
//! deliberately offers no atomic append spanning several streams.  The
//! contract-level statement of that rule is `OCC-3` in
//! [`nitinol_persistence::store::EventStore::append`]; the decision record for
//! *why* — prior art surveyed, and the conditions under which it could be
//! revisited — is the ADR at
//! <https://github.com/HalsekiRaika/nitinol/issues/74>.  Cross-stream
//! consistency is expressed with fact events plus at-least-once
//! process-manager delivery and idempotency.  This crate is that expression,
//! written out, on a payroll run that owes 32 employees a payslip.
//!
//! # The shape
//!
//! 1. **One fact event.**  [`payroll_run::PayrollRun`] records the whole
//!    approval as a single [`payroll_run::PayrollRunApproved`] on its own
//!    stream.  Several events in one stream are still one atomic write
//!    (`append(Vec)`), so a decision that fits in one stream never needs more
//!    than nitinol offers.
//! 2. **Fan-out by process manager.**  [`saga::FanOutSaga`], run under a
//!    `SagaManagerProps` manager, reacts to that fact and dispatches one issue
//!    command per employee, at-least-once, through the outbox.
//! 3. **Idempotent creation.**  [`payslip::Payslip`] answers a repeated
//!    [`payslip::IssuePayslip`] with "already issued" instead of a second
//!    write.  A creation-only fan-out therefore needs no compensation: the
//!    recovery for an interrupted fan-out is to run it again.
//!
//! # One store, many streams
//!
//! [`nitinol_persistence::store::EventStore`] is keyed by stream —
//! `append(key, events)` and `load(LoadQuery)` both name the stream they act
//! on — so a single instance holds the payroll run's stream, the 32 payslip
//! streams and the saga's own journal side by side, each owner reading only its
//! own key.  This crate wires exactly one store and hands the same
//! `Arc<dyn EventStore>` to every spawn: the run aggregate, the payslip
//! registry, the saga journal, and the manager's subscription.
//!
//! A subscription does not need a store of its own either.  The manager is
//! given a [`nitinol_eventsource::SequenceCursor`], and
//! `SequenceCursor::Stream { key, .. }` filters the shared store down to the
//! run's stream by key; `SequenceCursor::Global` is the choice for a
//! subscription that wants everything in commit order.  Splitting streams
//! across store instances is a deployment decision — separate backends,
//! separate retention — not something the pattern requires.
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
//! # Modelling the in-between
//!
//! Between the fact event and the last payslip there is a real intermediate
//! state, and the pattern's answer is to *not* store it a second time.  Each
//! payslip's own stream already says whether it exists, so the fan-out's
//! progress is a query over those streams.  [`saga::FanOutSaga`] keeps no
//! counter: a counter would be a second owner of the same fact, and a crash
//! between the two writes would leave them disagreeing with no way to tell
//! which is right.
//!
//! What the saga's own stream does hold is its decision
//! ([`saga::FanOutStarted`]) and one outbox marker per dispatched command,
//! committed in the same append — that is durable *intent*, not duplicated
//! state, and it is what lets a restarted incarnation finish a dispatch it
//! never got to.
//!
//! # Make the trigger event self-sufficient
//!
//! `Saga::correlate` is handed the decoded event and nothing else — not the
//! stream key it was read from, not its sequence.  An event that leaves out
//! what its consumers need forces every consumer to reach back to where it came
//! from, and a consumer that cannot (a projection replaying an archive, a
//! second saga on a different subscription) simply cannot act on it.  So
//! [`payroll_run::PayrollRunApproved`] carries its own `payroll_run` key and
//! the full employee list — together they are what
//! [`payslip::Payslip::stream_key`] needs — and
//! [`payslip::IssuePayslip`] carries the stream key of its target, which is
//! also what makes it reconstructible from the outbox marker after a crash.
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
//!   one record and fans out to 32 payslips; the same fact delivered a second
//!   time leaves every payslip stream byte-identical; an issue replayed all the
//!   way down to the store conflicts on the genesis sequence while leaving the
//!   first write intact; and two runs covering one employee address separate
//!   payslip streams.
//! * `tests/crash_replay_completes_fanout.rs` — a fan-out interrupted part-way
//!   is completed by replay, without rewriting the payslips that already
//!   existed.

pub mod codec;
pub mod payroll_run;
pub mod payslip;
pub mod router;
pub mod saga;
