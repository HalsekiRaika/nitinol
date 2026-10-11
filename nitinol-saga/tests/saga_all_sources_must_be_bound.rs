//! Every declared external source needs a subscription; a declared deadline
//! needs none.
//!
//! ```gherkin
//!   Scenario: all external sources bound, deadline declared
//!     Given a saga with three external variants and one deadline variant
//!     When all three external sources are bound
//!     Then spawn is available and every source is delivered
//!
//!   Scenario: not every source bound
//!     Given a saga with two external variants
//!     When only one source is bound and spawn is called
//!     Then it is a compile error
//! ```
//!
//! ## What this file carries, and what it cannot
//!
//! The first scenario is here in full: three sources bound, `spawn` available,
//! all three delivered, and the deadline variant costing no fourth binding.
//! That the deadline variant costs nothing is the half that would otherwise go
//! unnoticed — an implementation that allotted a binding slot to every variant
//! would make this saga unspawnable, which is exactly what the test would
//! report.
//!
//! The second scenario is a compile error, and this workspace has no
//! compile-fail harness (`trybuild`) for integration tests: a binary cannot
//! assert that another program fails to build.  It is pinned instead by the
//! `compile_fail` doctests on `SagaSpawn::spawn`, alongside the ones for
//! binding nothing at all, binding one source twice, and binding a source the
//! alphabet never declared.
//! See: nitinol-saga/src/system_ext.rs
//!
//! ## API contract assumed by this file
//!
//! - `subscribed_to::<E>(Subscription)` moves one declared source's binding
//!   slot, and `spawn` exists only once every external source's slot is filled.
//! - A `#[subscribe(deadline)]` variant is allotted no slot.
//!
//! Run with: `cargo test -p nitinol-saga --test saga_all_sources_must_be_bound`.

#[path = "common/helpers.rs"]
mod common;
use common::JsonCodec;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;
use serde::{Deserialize, Serialize};

use nitinol::saga::SagaSubscribe;
use nitinol_eventsource::codec::Codec;
use nitinol_eventsource::Event;
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::{AggregateId, AppendingEvent, EventType, Family, TypeName};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{
    Deadline, DeadlineMessage, Saga, SagaContext, SagaEffect, SagaId, SagaProps, Subscription,
};

// Domain — three external sources and one deadline.

macro_rules! declare_source {
    ($name:ident, $type_name:literal) => {
        #[derive(Clone, Debug, Serialize, Deserialize)]
        struct $name {
            order: String,
        }

        impl Event for $name {
            const EVENT_TYPE: EventType =
                EventType::new(Family::new("bind_all"), TypeName::new($type_name));
            type Codec = JsonCodec;
        }
    };
}

declare_source!(Placed, "Placed");
declare_source!(Reserved, "Reserved");
declare_source!(Shipped, "Shipped");

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Noted {
    label: String,
}

impl Event for Noted {
    const EVENT_TYPE: EventType = EventType::new(Family::new("bind_all"), TypeName::new("Noted"));
    type Codec = JsonCodec;
}

#[derive(Clone, Debug, PartialEq)]
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

/// Three external sources and the saga's own time.  Only the first three ask
/// for a subscription.
// The observed payload is decoded and delivered as the variant it belongs to;
// what this saga decides does not read it, which `dead_code` cannot tell apart
// from a field nothing fills.
#[allow(dead_code)]
#[derive(SagaSubscribe)]
enum ThreeSourcesAndTime {
    Placed(Placed),
    Reserved(Reserved),
    Shipped(Shipped),
    #[subscribe(deadline)]
    Reminder(Reminder),
}

const ORDER: &str = "ord-1";
const SAGA_ID: &str = "bind-all-saga";

type Handled = Arc<Mutex<Vec<&'static str>>>;

struct ThreeSourceSaga {
    /// Held so the deadline variant is more than a shape: the component is the
    /// reason the variant is declared, and it still costs no binding.
    deadline: Deadline<Reminder>,
    handled: Handled,
}

#[async_trait]
impl Saga for ThreeSourceSaga {
    type Subscribe = ThreeSourcesAndTime;
    type Event = Noted;
    type Error = std::convert::Infallible;

    fn correlate(input: &Self::Subscribe) -> Option<SagaId> {
        match input {
            ThreeSourcesAndTime::Placed(_)
            | ThreeSourcesAndTime::Reserved(_)
            | ThreeSourcesAndTime::Shipped(_) => Some(SagaId::new(SAGA_ID)),
            ThreeSourcesAndTime::Reminder(_) => None,
        }
    }

    async fn handle(
        &mut self,
        input: Self::Subscribe,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        let variant = match input {
            ThreeSourcesAndTime::Placed(_) => "Placed",
            ThreeSourcesAndTime::Reserved(_) => "Reserved",
            ThreeSourcesAndTime::Shipped(_) => "Shipped",
            ThreeSourcesAndTime::Reminder(_) => "Reminder",
        };
        self.handled
            .lock()
            .expect("handled buffer must not be poisoned")
            .push(variant);
        // The component is used here so the deadline arm is reachable in
        // principle; this saga never sets one, so it never is.
        let _ = &self.deadline;
        Ok(SagaEffect::persist(Noted {
            label: variant.to_owned(),
        }))
    }
}

// Helpers

async fn append(
    store: &Arc<dyn EventStore>,
    stream: &AggregateId,
    event_type: EventType,
    order: &str,
) {
    #[derive(Serialize)]
    struct Wire<'a> {
        order: &'a str,
    }
    store
        .append(
            stream.as_str(),
            vec![AppendingEvent {
                sequence: 1,
                event_type,
                payload: serde_json::to_vec(&Wire { order })
                    .map(Bytes::from)
                    .expect("encode an upstream record must succeed"),
                occurred_at: jiff::Timestamp::now(),
            }],
        )
        .await
        .expect("appending an upstream record must succeed");
}

const BARRIER_TIMEOUT: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(25);

async fn wait_for_handled(handled: &Handled, expected: usize) -> Vec<&'static str> {
    let deadline = Instant::now() + BARRIER_TIMEOUT;
    loop {
        let current = handled
            .lock()
            .expect("handled buffer must not be poisoned")
            .clone();
        if current.len() >= expected {
            return current;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {expected} handle invocations; saw {current:?}"
        );
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

// Tests

/// Given a saga declaring three external sources and one deadline,
/// When all three external sources are bound,
/// Then `spawn` is available and every bound source is delivered — the deadline
/// variant asked for no binding of its own.
#[tokio::test]
async fn binding_every_external_source_is_enough_even_with_a_deadline_declared() {
    let ps = ProcessSystem::new().await;

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let placed_stream = AggregateId::new("bind-all-placed");
    let reserved_stream = AggregateId::new("bind-all-reserved");
    let shipped_stream = AggregateId::new("bind-all-shipped");
    append(&upstream_store, &placed_stream, Placed::EVENT_TYPE, ORDER).await;
    append(
        &upstream_store,
        &reserved_stream,
        Reserved::EVENT_TYPE,
        ORDER,
    )
    .await;
    append(&upstream_store, &shipped_stream, Shipped::EVENT_TYPE, ORDER).await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let handled: Handled = Arc::new(Mutex::new(Vec::new()));

    let handled_for_producer = Arc::clone(&handled);
    let _proxy = SagaProps::<ThreeSourceSaga>::new(
        SagaId::new(SAGA_ID),
        Arc::clone(&saga_store),
        move |components| ThreeSourceSaga {
            deadline: components.deadline(),
            handled: Arc::clone(&handled_for_producer),
        },
    )
    .subscribed_to::<Placed>(Subscription::stream(&upstream_store, &placed_stream))
    .subscribed_to::<Reserved>(Subscription::stream(&upstream_store, &reserved_stream))
    .subscribed_to::<Shipped>(Subscription::stream(&upstream_store, &shipped_stream))
    .spawn(&ps)
    .await;

    let mut seen = wait_for_handled(&handled, 3).await;
    seen.sort();

    assert_eq!(
        seen,
        vec!["Placed", "Reserved", "Shipped"],
        "binding each declared external source must be the whole requirement: a \
         deadline variant that also demanded a binding would leave this saga \
         unspawnable"
    );
}
