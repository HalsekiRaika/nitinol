//! `Saga::apply` is optional, and the saga's own event stream is not.
//!
//! ```gherkin
//!   Scenario: a saga that does not implement apply compiles
//!     Given a stateless saga with no apply implementation
//!     Then it compiles
//!     And the decision events handle returns are persisted on its own stream
//! ```
//!
//! ## What carries each half
//!
//! "Compiles" is carried by this binary building at all: `StatelessSaga`
//! implements every item the trait still *requires* and no `apply`.  While
//! `apply` has no default body the impl is incomplete (E0046) and this binary
//! fails to build — which is equally true of a default body that panics, since
//! the runtime test below drives the replay path that would reach it.
//!
//! "The own stream is maintained" is the runtime half, and it is the reason
//! `apply` becoming optional is not the same thing as the stream becoming
//! optional: the stream is what the delivery host consumes and what the
//! consumed position is folded out of, so a stateless saga still journals its
//! decisions.  The assertion reads the stream, not a method call.
//!
//! ## API contract assumed by this file
//!
//! - `#[derive(SagaSubscribe)]`, `Saga::Subscribe`, `Saga::correlate`,
//!   `Saga::handle`, `SagaManagerProps::new(Fn(SagaComponents<S>) -> S)`,
//!   `observing::<E>(Subscription)` — see
//!   `saga_multi_source_observation.rs` for the full assumed surface.
//!
//! Run with: `cargo test -p nitinol-saga --test saga_apply_is_optional`.

#[path = "common/helpers.rs"]
mod common;
use common::JsonCodec;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};

use nitinol::saga::SagaSubscribe;
use nitinol_eventsource::system::EventSourceSystem;
use nitinol_eventsource::Event;
use nitinol_persistence::provider::PersistenceProvider;
use nitinol_persistence::store::{InMemoryStore, Journal};
use nitinol_persistence::{
    AggregateId, AppendingEvent, EventType, Family, LoadQuery, LoadedEvent, TypeName,
};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{Saga, SagaContext, SagaEffect, SagaId, SagaManagerProps, Subscription};

// Domain

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Requested {
    order: String,
}

impl Event for Requested {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("apply_optional"), TypeName::new("Requested"));
    type Codec = JsonCodec;
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Translated {
    order: String,
}

impl Event for Translated {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("apply_optional"), TypeName::new("Translated"));
    type Codec = JsonCodec;
}

#[derive(SagaSubscribe)]
enum Incoming {
    Requested(Requested),
}

const SAGA_ID: &str = "apply-optional-saga";

/// A translating saga: it holds no state, so it has nothing for `apply` to do
/// and declares none.
struct StatelessSaga {
    handled: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl Saga for StatelessSaga {
    type Subscribe = Incoming;
    type Event = Translated;
    type Error = std::convert::Infallible;

    fn correlate(_input: &Self::Subscribe) -> Option<SagaId> {
        Some(SagaId::new(SAGA_ID))
    }

    async fn handle(
        &mut self,
        input: Self::Subscribe,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        let Incoming::Requested(event) = input;
        self.handled
            .lock()
            .expect("handled buffer must not be poisoned")
            .push(event.order.clone());
        Ok(SagaEffect::persist(Translated { order: event.order }))
    }
}

// Helpers

async fn append_requested(store: &Journal, stream: &AggregateId, sequence: u64, order: &str) {
    store
        .append(
            stream.as_str(),
            vec![AppendingEvent {
                sequence,
                event_type: Requested::EVENT_TYPE,
                payload: serde_json::to_vec(&Requested {
                    order: order.to_owned(),
                })
                .map(Bytes::from)
                .expect("encode Requested must succeed"),
                occurred_at: jiff::Timestamp::now(),
            }],
        )
        .await
        .expect("append Requested must succeed");
}

async fn load_saga_events(store: &Journal, saga_id: &SagaId) -> Vec<LoadedEvent> {
    store
        .load(LoadQuery::by_stream(saga_id))
        .await
        .expect("load saga stream must succeed")
        .try_collect()
        .await
        .expect("collect saga events must succeed")
}

const BARRIER_TIMEOUT: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(25);

async fn wait_for_decisions(store: &Journal, saga_id: &SagaId, expected: usize) -> Vec<Translated> {
    let deadline = Instant::now() + BARRIER_TIMEOUT;
    loop {
        let decisions: Vec<Translated> = load_saga_events(store, saga_id)
            .await
            .iter()
            .filter(|event| event.event_type == Translated::EVENT_TYPE)
            .map(|event| {
                serde_json::from_slice::<Translated>(&event.payload)
                    .expect("the saga's own event must decode")
            })
            .collect();
        if decisions.len() >= expected {
            return decisions;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {expected} decisions on the saga's own stream; \
             saw {decisions:?}"
        );
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

// Tests

/// Acceptance scenario: a saga that implements no `apply` compiles, and its
/// decisions still reach its own stream.
#[tokio::test]
async fn a_saga_without_apply_still_journals_its_decisions() {
    let system = EventSourceSystem::builder(ProcessSystem::new().await)
        .with_persistence(PersistenceProvider::new(InMemoryStore::default()))
        .build();
    let journal = system.persistence().event_store();

    let upstream = AggregateId::new("apply-optional-upstream");
    append_requested(&journal, &upstream, 1, "ord-1").await;
    append_requested(&journal, &upstream, 2, "ord-2").await;

    let saga_id = SagaId::new(SAGA_ID);
    let handled = Arc::new(Mutex::new(Vec::<String>::new()));

    let handled_for_producer = Arc::clone(&handled);
    let _proxy = SagaManagerProps::<StatelessSaga>::new(move |_components| StatelessSaga {
        handled: Arc::clone(&handled_for_producer),
    })
    .observing::<Requested>(Subscription::stream(&upstream))
    .spawn(&system)
    .await;

    let decisions = wait_for_decisions(&journal, &saga_id, 2).await;

    assert_eq!(
        decisions,
        vec![
            Translated {
                order: "ord-1".to_owned()
            },
            Translated {
                order: "ord-2".to_owned()
            },
        ],
        "a saga that implements no `apply` must still have its decision events \
         persisted on its own stream — the stream is what the delivery host \
         consumes and what the consumed position is folded out of"
    );
}

/// Given a saga with no `apply`, whose own stream already holds its decisions,
/// When a fresh incarnation replays that stream,
/// Then replay completes and the already-consumed input is not decided again:
/// the default `apply` is a no-op, not an unimplemented stub.
#[tokio::test]
async fn replay_over_a_saga_without_apply_adds_no_second_decision() {
    let system = EventSourceSystem::builder(ProcessSystem::new().await)
        .with_persistence(PersistenceProvider::new(InMemoryStore::default()))
        .build();
    let journal = system.persistence().event_store();

    let upstream = AggregateId::new("apply-optional-replay-upstream");
    append_requested(&journal, &upstream, 1, "ord-1").await;

    let saga_id = SagaId::new(SAGA_ID);
    let handled = Arc::new(Mutex::new(Vec::<String>::new()));

    let handled_for_first = Arc::clone(&handled);
    let first = SagaManagerProps::<StatelessSaga>::new(move |_components| StatelessSaga {
        handled: Arc::clone(&handled_for_first),
    })
    .observing::<Requested>(Subscription::stream(&upstream))
    .spawn(&system)
    .await;

    wait_for_decisions(&journal, &saga_id, 1).await;
    first.manager().stop().await.expect("stop must succeed");
    drop(first);

    let handled_for_second = Arc::clone(&handled);
    let _second = SagaManagerProps::<StatelessSaga>::new(move |_components| StatelessSaga {
        handled: Arc::clone(&handled_for_second),
    })
    .observing::<Requested>(Subscription::stream(&upstream))
    .spawn(&system)
    .await;

    // Upstream seq 2 is the barrier: the poller delivers ascending and asks one
    // record at a time, so once seq 2 has been decided, seq 1's fate is settled.
    append_requested(&journal, &upstream, 2, "ord-2").await;
    let decisions = wait_for_decisions(&journal, &saga_id, 2).await;

    assert_eq!(
        decisions,
        vec![
            Translated {
                order: "ord-1".to_owned()
            },
            Translated {
                order: "ord-2".to_owned()
            },
        ],
        "replaying a saga's own stream through the default no-op `apply` must \
         complete and leave the consumed input consumed"
    );
    assert_eq!(
        handled
            .lock()
            .expect("handled buffer must not be poisoned")
            .clone(),
        vec!["ord-1".to_owned(), "ord-2".to_owned()],
        "each input reaches `handle` once across both incarnations"
    );
}
