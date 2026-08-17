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
//! written out.
//!
//! # The shape
//!
//! 1. **One fact event.**  [`batch::Batch`] records the whole decision as a
//!    single [`batch::BatchDecomposed`] on its own stream.  Several events in
//!    one stream are still one atomic write (`append(Vec)`), so a decision that
//!    fits in one stream never needs more than nitinol offers.
//! 2. **Fan-out by process manager.**  [`saga::FanOutSaga`], run under a
//!    `SagaManagerProps` manager, reacts to that fact and dispatches one
//!    creation command per child, at-least-once, through the outbox.
//! 3. **Idempotent creation.**  [`item::Item`] answers a repeated `CreateItem`
//!    with "already created" instead of a second write.  A creation-only
//!    fan-out therefore needs no compensation: the recovery for an interrupted
//!    fan-out is to run it again.
//!
//! # Choosing the stream that owns the decision
//!
//! The owner is the stream whose invariant the decision belongs to — the one
//! that would be wrong if the decision were recorded twice or not at all.  Here
//! that is the batch: "this batch was decomposed into these 32 items" is a fact
//! about the batch, and each child's existence is a consequence of it, not part
//! of it.  Two questions settle most cases:
//!
//! * *Which single stream must reject a contradictory second decision?*  That
//!   stream owns it.  If the answer is "several streams together", the boundary
//!   is drawn in the wrong place — an aggregate is the consistency boundary.
//! * *Can the consequence be re-derived from the decision alone?*  If yes, the
//!   consequence belongs downstream of a fact event, not inside the same write.
//!
//! # Modelling the in-between
//!
//! Between the fact event and the last child there is a real intermediate
//! state, and the pattern's answer is to *not* store it a second time.  Each
//! child's own stream already says whether it exists, so the fan-out's progress
//! is a query over those streams.  [`saga::FanOutSaga`] keeps no counter: a
//! counter would be a second owner of the same fact, and a crash between the
//! two writes would leave them disagreeing with no way to tell which is right.
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
//! [`batch::BatchDecomposed`] carries its own `batch` key and the full item
//! list, and [`item::CreateItem`] carries the stream key of its target — which
//! is also what makes it reconstructible from the outbox marker after a crash.
//!
//! # Two further steps, not implemented here
//!
//! * **Genesis by reference.**  If a child's initial state is derivable from
//!   the fact event, the child stream need not be written at fan-out time at
//!   all: creation can be deferred to the first real command, with the fact
//!   event as the reference for what the child started as.  That trades 32
//!   writes for none, at the cost of a child that is not yet visible in the
//!   store.
//! * **Closing the books.**  If a reader needs "all 32 arrived" as a single
//!   fact, express it as one summary event written once the count is reached,
//!   rather than by making readers aggregate 32 streams themselves.
//!
//! # What the tests fix
//!
//! * `tests/redelivery_is_noop.rs` — one decision reaches the batch stream as
//!   one record and fans out to 32 children; the same fact delivered a second
//!   time leaves every child stream byte-identical; and a creation replayed all
//!   the way down to the store conflicts on the genesis sequence while leaving
//!   the first write intact.
//! * `tests/crash_replay_completes_fanout.rs` — a fan-out interrupted part-way
//!   is completed by replay, without rewriting the children that already
//!   existed.

pub mod batch;
pub mod codec;
pub mod item;
pub mod router;
pub mod saga;
