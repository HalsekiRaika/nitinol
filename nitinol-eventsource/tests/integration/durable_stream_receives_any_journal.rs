//! The two durable-stream entry points are *handed* the journal they read,
//! rather than naming one when they are built.
//!
//! ```gherkin
//!   Scenario: a stream is handed its journal rather than naming one
//!     Given DurableStream::new and DurableSubscription::new
//!     When each is named as a function value with its whole signature
//!     Then neither receives a journal
//!
//!   Scenario: a stream's journal arrives at the spawn
//!     Given a stream built from props and a stream taken from a system
//!     When each is spawned
//!     Then the props-built one is handed the system, and the system-built one
//!          needs nothing re-injected
//!
//!   Scenario: a subscription's journal arrives at the spawn
//!     Given a direct-poller configuration
//!     When spawn_child, spawn_child_for and spawn_from are named
//!     Then each receives the journal as an argument of that spawn
//!
//!   Scenario: the journal handed over is the one a system distributes
//!     Given a system over any store
//!     When the wiring asks it for the journal a spawn receives
//!     Then the store's own type is already gone from the answer
//! ```
//!
//! These are the two entry points a deployment reaches when it wires its own
//! polling: [`DurableStream`] for the shared fan-out, [`DurableSubscription`]
//! for a subscriber's own direct poller. Neither carries a journal any more —
//! what they hold is how to read a record and how often to ask — so there is
//! one place a destination can be named, and it is the system.
//!
//! What is settled here is the receiving shape, and settled by coercion: a
//! function value names a signature in full, so a store parameter returning to
//! either constructor fails here rather than drifting past the clause. That the
//! poller then reads *the journal it was handed* — catchup, live tail, cursor
//! advance — is the subject of `durable_stream.rs`, which drives both ends of
//! the one store a system was built on; repeating that here would be the same
//! failure observed twice.

use nitinol_eventsource::system::EventSourceSystem;
use nitinol_eventsource::{
    CursorSet, CursorUnset, DurableStream, DurableStreamProxy, DurableSubscription, SequenceCursor,
};
use nitinol_persistence::provider::Store;
use nitinol_persistence::store::Journal;
use nitinol_persistence::LoadedEvent;
use nitinol_runtime::error::SpawnError;
use nitinol_runtime::ident::ProcessName;
use nitinol_runtime::process::{Process, ProcessContext, ProcessProxy, Receive};
use nitinol_runtime::ProcessSystem;

/// The payload these clauses transform into; its value never matters.
type Payload = u64;

/// A transform named as a function pointer so the entry points' generic
/// parameter is pinned to one concrete type in the assertions below.
type Transform = fn(LoadedEvent) -> Option<Payload>;

/// A transform that carries nothing, for the clauses that need a value of
/// [`Transform`] rather than only its type.
fn skip_everything(_loaded: LoadedEvent) -> Option<Payload> {
    None
}

/// `DurableStream::new` MUST receive a topic and a transform, and nothing else.
///
/// Not a `#[test]` — the compiler enforces the signatures at build time. A
/// journal parameter added back to the constructor, in any of the forms it used
/// to accept, is a compile error here.
#[allow(dead_code)]
fn _assert_sig_durable_stream_new_names_no_journal() {
    let _: fn(ProcessName, Transform) -> DurableStream<Payload, CursorUnset> = DurableStream::new;
}

/// The same for the direct-poller configuration: a transform alone.
#[allow(dead_code)]
fn _assert_sig_durable_subscription_new_names_no_journal() {
    let _: fn(Transform) -> DurableSubscription<Payload> = DurableSubscription::new;
}

/// A stream built from props is handed a system at its spawn, and takes the
/// journal from there.
#[allow(dead_code)]
async fn _assert_sig_a_props_built_streams_journal_arrives_at_the_spawn<St: Store>(
    stream: DurableStream<Payload, CursorSet>,
    system: &EventSourceSystem<St>,
) -> Result<DurableStreamProxy<Payload>, SpawnError> {
    stream.spawn(system).await
}

/// A stream taken from a system carries that system already, so its spawn
/// receives nothing.
#[allow(dead_code)]
async fn _assert_sig_a_system_built_streams_spawn_needs_nothing<St: Store>(
    system: &EventSourceSystem<St>,
    topic: ProcessName,
    cursor: SequenceCursor,
) -> Result<DurableStreamProxy<Payload>, SpawnError> {
    system
        .durable_stream::<Payload, _>(topic, skip_everything)
        .cursor(cursor)
        .spawn()
        .await
}

/// A subscription spawned as a child of the subscribing process receives the
/// journal at that spawn.
#[allow(dead_code)]
async fn _assert_sig_spawn_child_receives_the_journal<S>(
    subscription: &DurableSubscription<Payload>,
    ctx: &mut ProcessContext<S>,
    journal: Journal,
    cursor: SequenceCursor,
) where
    S: Process + Receive<Payload, Response = ()>,
{
    subscription.spawn_child(ctx, journal, cursor).await;
}

/// The same where the lifecycle owner and the subscriber are two processes.
#[allow(dead_code)]
async fn _assert_sig_spawn_child_for_receives_the_journal<Parent, Sub>(
    subscription: &DurableSubscription<Payload>,
    ctx: &mut ProcessContext<Parent>,
    journal: Journal,
    subscriber: ProcessProxy<Sub>,
    cursor: SequenceCursor,
    name: ProcessName,
) where
    Parent: Process,
    Sub: Process + Receive<Payload, Response = ()>,
{
    subscription
        .spawn_child_for(ctx, journal, subscriber, cursor, name)
        .await;
}

/// And the same from wiring outside the subscriber's own context.
#[allow(dead_code)]
async fn _assert_sig_spawn_from_receives_the_journal<S>(
    subscription: &DurableSubscription<Payload>,
    system: &ProcessSystem,
    journal: Journal,
    subscriber: ProcessProxy<S>,
    cursor: SequenceCursor,
    name: ProcessName,
) where
    S: Process + Receive<Payload, Response = ()>,
{
    subscription
        .spawn_from(system, journal, subscriber, cursor, name)
        .await;
}

/// Whoever hands a subscription its journal takes it from a system, and what a
/// system answers with is a [`Journal`] whatever store it was built on.
///
/// This is what makes the three spawns above reachable from wiring that was
/// written before the deployment's store was known: the type they receive is
/// the type every store erases its journal to.
#[allow(dead_code)]
fn _assert_sig_the_journal_a_spawn_receives_is_the_one_a_system_distributes<St: Store>(
    system: &EventSourceSystem<St>,
) -> Journal {
    system.persistence().event_store()
}
