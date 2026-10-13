//! A saga may declare a deadline variant and hold no `Deadline` component.
//!
//! ```gherkin
//!   Scenario: a deadline variant without the component
//!     Given a saga whose Subscribe enum has a #[subscribe(deadline)] variant
//!     And whose struct holds no Deadline component
//!     Then that saga compiles
//!     And there is no way for it to create a deadline-setting effect
//! ```
//!
//! ## What each half rests on
//!
//! "Compiles" is carried by this binary building at all, and by the saga below
//! actually running: its producer is `|_| ComponentlessSaga`, so it asks the
//! components for nothing.  Its deadline arm is still written — the `match` is
//! exhaustive or it is not a `match` — and is a visible dead branch, which the
//! counter below reports as never taken.
//!
//! "There is no way to create one" is a negative claim about the surface, and
//! rests on two things this file cannot assert by itself: `Deadline<M>` has no
//! public constructor, so the component is unobtainable outside the producer;
//! and no effect constructor builds a schedule fact without one, so there is no
//! second route.  Rust cannot express "this does not compile" from an
//! integration test — this workspace has no `trybuild` harness for them — so
//! both are pinned by `compile_fail` doctests instead: minting a `Deadline` or
//! defaulting one, and writing a `ScheduleSpec` down or reaching for the
//! effect constructor that used to build one.
//! See: nitinol-saga/src/components/deadline.rs, nitinol-saga/src/effect/core.rs
//!
//! Asking the components for a deadline the alphabet never declared is pinned
//! the same way.
//! See: nitinol-saga/src/components.rs
//!
//! What *is* observed here is the consequence: a saga with the variant and
//! without the component runs normally, never reaching its deadline arm,
//! instead of failing to spawn or panicking on a half-wired timer.
//!
//! ## API contract assumed by this file
//!
//! - `#[derive(SagaSubscribe)]` accepts a `#[subscribe(deadline)]` variant
//!   whether or not the saga holds a `Deadline<M>`.
//! - `SagaManagerProps::new(store, Fn(SagaComponents<S>) -> S)` calls the
//!   producer with the components; a producer is free to ignore them.
//!
//! Run with:
//! `cargo test -p nitinol-saga --test saga_deadline_requires_component`.

#[path = "common/helpers.rs"]
mod common;
use common::JsonCodec;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;

use nitinol::saga::SagaSubscribe;
use nitinol_eventsource::codec::Codec;
use nitinol_eventsource::Event;
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::{AppendingEvent, EventType, Family, TypeName};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{
    DeadlineMessage, Saga, SagaContext, SagaEffect, SagaId, SagaManagerProps, Subscription,
};
use serde::{Deserialize, Serialize};

// Domain

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Nudge {
    note: String,
}

impl Event for Nudge {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("deadline_componentless"),
        TypeName::new("Nudge"),
    );
    type Codec = JsonCodec;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Noted {
    note: String,
}

impl Event for Noted {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("deadline_componentless"),
        TypeName::new("Noted"),
    );
    type Codec = JsonCodec;
}

/// Declared as this saga's own time, and never used: the component that would
/// set one is not held.
#[derive(Clone, Debug)]
struct Reminder;

struct ReminderCodec;

impl Codec<Reminder> for ReminderCodec {
    type Error = std::convert::Infallible;

    fn encode(_value: &Reminder) -> Result<Bytes, Self::Error> {
        Ok(Bytes::new())
    }

    fn decode(_payload: &[u8]) -> Result<Reminder, Self::Error> {
        Ok(Reminder)
    }
}

impl DeadlineMessage for Reminder {
    const TYPE_NAME: &'static str = "reminder";
    const CODEC_NAME: &'static str = "reminder-codec";
    type Codec = ReminderCodec;
}

#[derive(SagaSubscribe)]
enum Declared {
    Nudge(Nudge),
    #[subscribe(deadline)]
    Reminder(Reminder),
}

/// How many times the dead deadline arm was taken.  Binary-wide; it must stay
/// at zero, because no component exists to have set a deadline.
static DEAD_ARM_TAKEN: AtomicUsize = AtomicUsize::new(0);

const SAGA_ID: &str = "deadline-componentless-saga";
const UPSTREAM: &str = "deadline-componentless-upstream";

/// Declares a deadline variant; holds nothing to set one with.
struct ComponentlessSaga {
    handled: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl Saga for ComponentlessSaga {
    type Subscribe = Declared;
    type Event = Noted;
    type Error = std::convert::Infallible;

    fn correlate(input: &Self::Subscribe) -> Option<SagaId> {
        match input {
            Declared::Nudge(_) => Some(SagaId::new(SAGA_ID)),
            Declared::Reminder(_) => None,
        }
    }

    async fn handle(
        &mut self,
        input: Self::Subscribe,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        match input {
            Declared::Nudge(nudge) => {
                self.handled
                    .lock()
                    .expect("handled buffer must not be poisoned")
                    .push(nudge.note.clone());
                Ok(SagaEffect::persist(Noted { note: nudge.note }))
            }
            // Unreachable by construction: nothing in this saga can set a
            // deadline, so nothing can fire one.  The arm exists because the
            // alphabet declares the variant.
            Declared::Reminder(_) => {
                DEAD_ARM_TAKEN.fetch_add(1, Ordering::SeqCst);
                Ok(SagaEffect::None)
            }
        }
    }
}

// Helpers

async fn append_nudge(store: &Arc<dyn EventStore>, sequence: u64, note: &str) {
    store
        .append(
            UPSTREAM,
            vec![AppendingEvent {
                sequence,
                event_type: Nudge::EVENT_TYPE,
                payload: serde_json::to_vec(&Nudge {
                    note: note.to_owned(),
                })
                .map(Bytes::from)
                .expect("encode Nudge must succeed"),
                occurred_at: jiff::Timestamp::now(),
            }],
        )
        .await
        .expect("append Nudge must succeed");
}

// Tests

/// Acceptance scenario: a saga that declares its own time and holds no
/// component for it spawns, runs, and never reaches the arm it cannot drive.
#[tokio::test]
async fn a_saga_declaring_a_deadline_without_the_component_runs_normally() {
    let ps = ProcessSystem::new().await;

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    append_nudge(&upstream_store, 1, "first").await;
    append_nudge(&upstream_store, 2, "second").await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let handled = Arc::new(Mutex::new(Vec::<String>::new()));

    let handled_for_producer = Arc::clone(&handled);
    // The whole of the producer: the components are offered and declined.
    let _proxy =
        SagaManagerProps::<ComponentlessSaga>::new(Arc::clone(&saga_store), move |_components| {
            ComponentlessSaga {
                handled: Arc::clone(&handled_for_producer),
            }
        })
        .observing::<Nudge>(Subscription::stream(&upstream_store, UPSTREAM))
        .spawn(&ps)
        .await;

    // Upstream seq 2 is the barrier: the poller delivers ascending and asks one
    // record at a time, so once it has been handled the incarnation is settled.
    let deadline = Instant::now() + Duration::from_secs(5);
    let seen = loop {
        let current = handled
            .lock()
            .expect("handled buffer must not be poisoned")
            .clone();
        if current.len() >= 2 {
            break current;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for both upstream records; saw {current:?}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    };

    assert_eq!(
        seen,
        vec!["first".to_owned(), "second".to_owned()],
        "declaring a deadline variant must not cost the saga its external \
         source: a saga with the variant and no component is an ordinary saga"
    );
    assert_eq!(
        DEAD_ARM_TAKEN.load(Ordering::SeqCst),
        0,
        "with no component there is no way to set a deadline, so the deadline \
         arm must stay a visible dead branch"
    );
}
