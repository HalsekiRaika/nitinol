//! The two durable-stream entry points receive the journal as
//! `impl Into<Journal>`.
//!
//! ```gherkin
//!   Scenario: a stream is built on a journal however the caller holds one
//!     Given a backend handle, a Journal, and a reference to that Journal
//!     When DurableStream::new and DurableSubscription::new are called on each
//!     Then all six calls compile
//!
//! ```
//!
//! These are the two entry points a deployment reaches when it wires its own
//! polling: [`DurableStream`] for the shared fan-out, [`DurableSubscription`]
//! for a subscriber's own direct poller. Both used to require the journal as a
//! shared handle, which made wrapping it the idiom for naming it beside the
//! store a system was built on.
//!
//! What is settled here is the receiving shape. That the poller then reads *the
//! journal it was built on* — catchup, live tail, cursor advance — is the
//! subject of `durable_stream.rs`, which drives both ends of the one store it
//! names; repeating that here would be the same failure observed twice.

use nitinol_eventsource::{DurableStream, DurableSubscription};
use nitinol_persistence::store::{InMemoryEventStore, Journal};
use nitinol_persistence::LoadedEvent;
use nitinol_runtime::ident::ProcessName;

/// The payload these clauses transform into; its value never matters.
type Payload = u64;

/// A transform named as a function pointer so the entry points' generic
/// parameter is pinned to one concrete type in the assertions below.
type Transform = fn(LoadedEvent) -> Option<Payload>;

/// `DurableStream::new` MUST receive the journal as `impl Into<Journal>`, which
/// is these three call shapes.
///
/// Not a `#[test]` — the compiler enforces the signatures at build time. A
/// return to a handle the framework shares for the caller, or a receiving shape
/// that takes only one of the three forms, is a compile error here.
#[allow(dead_code)]
fn _assert_sig_durable_stream_new_receives_any_form_of_a_journal<'a>(_held: &'a Journal) {
    let _: fn(ProcessName, InMemoryEventStore, Transform) -> DurableStream<Payload> =
        DurableStream::new;
    let _: fn(ProcessName, Journal, Transform) -> DurableStream<Payload> = DurableStream::new;
    let _: fn(ProcessName, &'a Journal, Transform) -> DurableStream<Payload> = DurableStream::new;
}

/// The same three shapes for the direct-poller configuration.
#[allow(dead_code)]
fn _assert_sig_durable_subscription_new_receives_any_form_of_a_journal<'a>(_held: &'a Journal) {
    let _: fn(InMemoryEventStore, Transform) -> DurableSubscription<Payload> =
        DurableSubscription::new;
    let _: fn(Journal, Transform) -> DurableSubscription<Payload> = DurableSubscription::new;
    let _: fn(&'a Journal, Transform) -> DurableSubscription<Payload> = DurableSubscription::new;
}
