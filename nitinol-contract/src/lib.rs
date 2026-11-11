//! The contract between the `nitinol` runtime and downstream domain code.
//!
//! | Item | Purpose |
//! |---|---|
//! | [`Event`] | Marker for domain events; carries their persisted identity |
//! | [`Codec`] | The format a domain value is written to and read back from |
//! | [`Aggregate`] | Domain state holder; evolves state by applying events |
//! | [`Snapshotable`] | Opt-in snapshot support for faster replay |
//! | [`Recovery`] | What [`Aggregate::Recover`] may name: [`Never`] or [`Latest`] |
//! | [`Decider`] | Decides what a command means for the current state |
//! | [`Decision`] | A decider's conclusion: facts and an answer, or a refusal |
//! | [`Query`] | Asks the current state a question |
//! | [`EventType`], [`Family`], [`TypeName`] | The identity an event declares: its family and its type name |
//! | [`MaterializedPath`], [`ParsedEventType`], [`MaterializedPathParseError`] | The canonical path an identity is written down as, and how it is read back |
//! | [`EventRegistryEntry`] | How a declaration tells the machinery the path it is written down as |
//! | [`RESERVED_NAMESPACE`], [`is_within_reserved_namespace`] | The names the framework keeps for its own records |
//!
//! Every one of them is pure: no I/O, no `async`, and therefore no async
//! runtime. The identity types are plain values — `&'static str` and owned
//! strings — so naming an event depends on nothing a store or a runtime brings.
//! They live here rather than in `nitinol-eventsource` or
//! `nitinol-persistence` so that a domain-layer crate can define and
//! property-test its events, aggregates, decisions and queries against the
//! framework's contract without taking on the execution machinery — or Tokio —
//! that runs them. `nitinol-eventsource` re-exports the traits and
//! `nitinol-persistence` re-exports the identity types, so the framework-side
//! paths are unchanged.
//!
//! [`Codec`] is here for the same reason: [`Event`] and [`Snapshotable`] each
//! *name* the codec that writes their values down, so stating a format is part
//! of defining a domain type rather than part of wiring one up. What a failure
//! of that codec means to an interpreter — how it is classified, whether it is
//! worth retrying — stays in `nitinol-eventsource`, which is where the failure
//! is observed.
//!
//! The execution-side abstractions — the aggregate activation, the projection
//! layer, the error families — are deliberately *not* here: they describe how
//! the runtime drives an aggregate, not what an aggregate is or what it decides.
//!
//! # Laws
//!
//! A decision says what happened; it does not say who writes it down. Anything
//! that reads a [`Decision`] and carries it out — the event-sourced runtime, a
//! test harness, a future interpreter that does not exist yet — is an
//! *interpreter*. The laws below are what makes any two correct interpreters
//! observationally equivalent: a domain written against this contract cannot
//! tell them apart, and may therefore be tested against the cheapest one.
//!
//! Laws L-1 to L-4 constrain what a domain may write here. Laws L-5 to L-9
//! constrain the interpreter, and are stated here because they are the promises
//! a domain is entitled to rely on when it writes a decider — the contract
//! itself cannot enforce them.
//!
//! They are executable. `nitinol-conformance` ships a suite that drives its own
//! decider through any interpreter and reads the resulting stream back itself;
//! each law below names the clause of that suite which fixes it, and a broken
//! law is reported by name. What that crate makes checkable is the same claim
//! stated here — the type and law of a `Decider`, a `Query` and a `Decision` —
//! which is why the record of a law lives with the law rather than with any one
//! interpreter.
//!
//! - **L-1** — [`Decider::decide`] and [`Query::query`] are pure and
//!   deterministic: no I/O, no clock, no randomness. Replaying the same events
//!   into the same state must reach the same decision, or replay would not
//!   reconstruct the past. Fixed by `decisions_and_answers_are_reproducible`.
//! - **L-2** — the order of the events in [`Decision::Accept`] is the order
//!   [`Aggregate::apply`] receives them, and an interpreter persists them as a
//!   single atomic append. Events that do not commute would otherwise land in a
//!   stream that replays into a state the decider never described. Fixed by
//!   `the_facts_of_an_acceptance_land_together_and_in_order`.
//! - **L-3** — `Accept { events: [], output }` is a legitimate acceptance:
//!   nothing is appended, and the output is delivered as usual. This is how a
//!   command that finds its work already done stays idempotent without
//!   fabricating an event or borrowing the vocabulary of refusal. Fixed by
//!   `an_acceptance_with_no_facts_still_answers`.
//! - **L-4** — a [`Decision::Reject`] is accompanied by no persistence
//!   whatsoever. A refusal is a statement about a command, not a fact about the
//!   aggregate, and leaves no trace in the stream. Fixed by
//!   `a_refusal_leaves_no_trace_in_the_stream`.
//! - **L-5** — on the ask path the output is delivered exactly once. The tell
//!   path discards the output — nobody is waiting for it — but still surfaces a
//!   rejection observably, so that a command silently refused is not mistaken
//!   for one carried out. Fixed by
//!   `an_answer_is_delivered_once_and_a_told_refusal_is_surfaced`.
//! - **L-6** — `Rejection` carries domain-rule violations only. Infrastructure
//!   and concurrency-control failures are not verdicts on the command and are
//!   reported by the interpreter's own error family (`AskError` in
//!   `nitinol-eventsource`), so that a caller can tell "the domain refused
//!   this" from "this never reached the domain". Fixed by
//!   `a_domain_refusal_is_told_apart_from_a_failure_of_the_machinery`.
//! - **L-7** — when creation collides with an aggregate that already exists, an
//!   interpreter does not fabricate an output; it reports the collision
//!   (`AskError::AlreadyCreated`). No decision was reached, so there is no
//!   answer to deliver, and inventing one would let a caller believe it created
//!   what someone else did. Fixed by
//!   `a_creation_that_collides_is_reported_rather_than_answered`.
//! - **L-8** — `sequence` and `occurred_at` are the machine's coordinates. The
//!   interpreter assigns them when it persists, and neither the contract nor
//!   the domain observes them; a domain that needs a time or an order states it
//!   as its own fact in an event. Fixed by
//!   `sequence_and_time_belong_to_the_machine`.
//! - **L-9** — the aggregate identifier is a domain fact. State owns it, having
//!   received it through a creation event, rather than holding it as a handle
//!   the machinery passed in. Fixed by
//!   `the_holder_is_a_domain_fact_and_not_the_stream_key`.
//!
//! ## Event identity
//!
//! Laws ID-1 to ID-4 are what a domain promises the machinery about the names
//! its events are written down under. No conformance suite fixes them —
//! neither `nitinol-conformance` nor `nitinol-store-conformance` — since no
//! interpreter and no store is involved: a broken promise is refused while the
//! constant naming it is evaluated, and the tests of this crate fix the rest. Where the machinery enforces the promise, the test
//! named is that of the crate enforcing it.
//!
//! - **ID-1** — one declaration is written down under one canonical path. The
//!   path is the family's segments followed by the type name, and a type name
//!   is a single segment, so the last segment reads as the type name and every
//!   segment before it as the family, with no other reading
//!   ([`EventType`], [`Family::new`], [`TypeName::new`],
//!   [`ParsedEventType::from_path`]). Which arm of an enum a row holds is the
//!   payload's to say; the column names the declaration. Fixed by
//!   `one_canonical_path_decomposes_into_one_pair_of_components`,
//!   `a_canonical_path_is_the_family_segments_followed_by_the_type_name_alone`
//!   and `type_name_with_dot_panics`.
//! - **ID-2** — an [`EventType`]'s `Display` and `to_path().to_string()` are the
//!   same string, and it round-trips through `parse::<MaterializedPath>()` and
//!   [`ParsedEventType::from_path`]. That string is the only encoding both
//!   sides of the column agree on. Fixed by
//!   `path_canonical_string_equals_event_type_display`,
//!   `event_type_round_trips_as_materialized_path` and
//!   `parsed_event_type_round_trips_every_shape`.
//! - **ID-3** — one canonical path is claimed by one declaration. A
//!   declaration tells the machinery its path as an [`EventRegistryEntry`], and
//!   the machinery gathers the entries to restore a row's declaration from the
//!   path the row carries. Two declarations claiming one path fail the build.
//!   The contract states the obligation; the machinery enforces it, with an
//!   exported symbol per path and with the registry it builds. Fixed by
//!   `nitinol-persistence`'s
//!   `a_canonical_path_two_declarations_claim_fails_the_build_and_names_both`
//!   and `nitinol-macros`' `duplicate_canonical_path` compile-fail case.
//! - **ID-4** — `nitinol` and every name beneath it belong to the framework: a
//!   domain names neither an event family nor a stream key there
//!   ([`RESERVED_NAMESPACE`], [`is_within_reserved_namespace`]). The one law
//!   has two enforcement points, each placed with the side that knows the name:
//!   a family is refused by [`Family::new`] and [`EventRegistryEntry::new`]
//!   while the constant holding it is evaluated, here, and a stream key is
//!   refused by `reject_reserved_id` when the identifier is constructed, in
//!   `nitinol-persistence`. Fixed by
//!   `family_new_refuses_a_family_inside_the_reserved_namespace` and
//!   `event_registry_entry_new_refuses_an_event_type_of_a_reserved_family`
//!   here, and by `nitinol-persistence`'s
//!   `aggregate_id_refuses_a_name_inside_the_reserved_namespace`.
//!
//! # One home for the contract
//!
//! [`Decider`] is the only decision contract in the framework. The effectful
//! trait of the same name that `nitinol-eventsource` used to define — async,
//! handed a `Context`, returning an `Effect` ADT the runtime then interpreted —
//! is gone rather than kept alongside this one. Two deciders would have meant
//! two answers to "what does a command mean", and the effectful one answered it
//! by describing machinery: it let a decision perform I/O, reach for the
//! aggregate's identity and sequence number, and fan out into several appends,
//! none of which a domain rule needs and all of which a replay must reproduce.
//!
//! Nor is a new `nitinol-core` crate introduced to hold the pure contract: this
//! crate already *is* the runtime-free layer, and a second one would leave two
//! plausible homes for every future contract.

mod aggregate;
mod codec;
mod decider;
mod decision;
mod event;
mod event_registry_entry;
mod event_type;
mod materialized_path;
mod query;
mod recover;

/// The way the framework's own records carry the names the framework reserved,
/// and nothing a user writes.
///
/// [`Family::new`] and [`EventRegistryEntry::new`] refuse the `nitinol`
/// namespace, which is the point of reserving it. The framework persists
/// records under exactly those names, so it needs constructors that waive the
/// namespace rule — and only that rule. They are here because they build the
/// identity types with access no other crate has; `nitinol-persistence`
/// re-exports them beside the registry machinery the framework reaches them
/// with.
///
/// No stability is promised for anything below.
#[doc(hidden)]
pub mod __private {
    pub use crate::event_registry_entry::reserved_event_registry_entry;
    pub use crate::event_type::reserved_family;
}

pub use self::aggregate::{Aggregate, Snapshotable};
pub use self::codec::Codec;
pub use self::decider::Decider;
pub use self::decision::{Accepting, Decision};
pub use self::event::Event;
pub use self::event_registry_entry::EventRegistryEntry;
pub use self::event_type::{
    is_within_reserved_namespace, EventType, Family, ParsedEventType, TypeName, RESERVED_NAMESPACE,
};
pub use self::materialized_path::{MaterializedPath, MaterializedPathParseError};
// The declaration a stored `event_type` is matched against.  Hidden and not
// re-exported through the umbrella crate: a domain type is covered by
// declaring `Event`, and the framework's own records declare it by hand.
#[doc(hidden)]
pub use self::event::{DeclaresEventType, MatchesEventType};
pub use self::query::Query;
pub use self::recover::{Latest, Never, RecoverPolicy, Recovery};
