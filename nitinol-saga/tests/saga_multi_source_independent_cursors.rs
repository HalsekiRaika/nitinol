//! Each declared source carries its own cursor, and one source's stalling never
//! holds another back.
//!
//! ```gherkin
//!   Scenario: one source resumes where it left off, the other from its floor
//!     Given a saga that consumed source A up to seq 3 and nothing of source B
//!     When a fresh incarnation subscribes to both sources again
//!     Then source A delivers only what it has not consumed
//!     And source B delivers from its own configured floor
//!
//!   Scenario: a stalled source does not hold the other back
//!     Given a saga whose source A never yields a record
//!     When a record of source B arrives
//!     Then handle receives it
//! ```
//!
//! ## API contract assumed by this file
//!
//! - `#[derive(SagaSubscribe)]`, `Saga::Subscribe`, and
//!   `subscribed_to::<E>(Subscription)` once per declared source — see
//!   `saga_multi_source_observation.rs` for the full assumed surface.
//!
//! ## Why these are two separate claims
//!
//! "Per-source cursor" is about *where* a source resumes; "no head-of-line
//! blocking" is about *whether* a source makes progress while another does not.
//! A single shared cursor breaks the first silently — one source's position
//! covers the other's records — and the second loudly: nothing is delivered at
//! all while one stream stalls.  They fail separately, so they are asserted
//! separately.
//!
//! Run with:
//! `cargo test -p nitinol-saga --test saga_multi_source_independent_cursors`.

#[path = "common/helpers.rs"]
mod common;
use common::{JsonCodec, CONSUMED_POSITION_MARKER};

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};

use nitinol::saga::SagaSubscribe;
use nitinol_eventsource::Event;
use nitinol_persistence::error::{AppendError, LoadError};
use nitinol_persistence::store::{EventStore, EventStream, InMemoryEventStore};
use nitinol_persistence::{
    AggregateId, AppendOutcome, AppendingEvent, EventType, Family, LoadQuery, LoadedEvent, TypeName,
};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{Saga, SagaContext, SagaEffect, SagaId, SagaProps, Subscription};

// Domain

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Alpha {
    note: String,
}

impl Event for Alpha {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("independent_cursors"), TypeName::new("Alpha"));
    type Codec = JsonCodec;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Beta {
    note: String,
}

impl Event for Beta {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("independent_cursors"), TypeName::new("Beta"));
    type Codec = JsonCodec;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Noted {
    label: String,
}

impl Event for Noted {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("independent_cursors"), TypeName::new("Noted"));
    type Codec = JsonCodec;
}

#[derive(SagaSubscribe)]
enum TwoSources {
    First(Alpha),
    Second(Beta),
}

const SAGA_ID: &str = "independent-cursors-saga";

/// What one `handle` invocation saw: the source it came from, as the variant,
/// plus that source's own sequence.  A per-source cursor is a claim about
/// *which sequences of which source* arrive, so both halves are recorded.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Seen {
    variant: &'static str,
    sequence: u64,
}

type Handled = Arc<Mutex<Vec<Seen>>>;

struct TwoSourceSaga {
    handled: Handled,
}

#[async_trait]
impl Saga for TwoSourceSaga {
    type Subscribe = TwoSources;
    type Event = Noted;
    type Error = std::convert::Infallible;

    fn correlate(_input: &Self::Subscribe) -> Option<SagaId> {
        Some(SagaId::new(SAGA_ID))
    }

    async fn handle(
        &mut self,
        input: Self::Subscribe,
        ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        let (variant, note) = match input {
            TwoSources::First(event) => ("First", event.note),
            TwoSources::Second(event) => ("Second", event.note),
        };
        self.handled
            .lock()
            .expect("handled buffer must not be poisoned")
            .push(Seen {
                variant,
                sequence: ctx.upstream_sequence(),
            });
        Ok(SagaEffect::persist(Noted {
            label: format!("{variant}:{note}"),
        }))
    }
}

// Stores

/// Never yields a record for `stalled_key`, and serves every other stream
/// normally.
///
/// Models a source that is stuck rather than empty: an empty stream still
/// completes, so a shared poller would move on from it.  A stream that neither
/// yields nor ends is what actually holds a shared cursor in place.
struct StallingStore {
    inner: InMemoryEventStore,
    stalled_key: String,
}

#[async_trait]
impl EventStore for StallingStore {
    async fn append(
        &self,
        key: &str,
        events: Vec<AppendingEvent>,
    ) -> Result<AppendOutcome, AppendError> {
        self.inner.append(key, events).await
    }

    async fn load(&self, query: LoadQuery) -> Result<EventStream<'_>, LoadError> {
        if query.stream_key.as_deref() == Some(self.stalled_key.as_str()) {
            return Ok(Box::pin(futures_util::stream::pending()));
        }
        self.inner.load(query).await
    }
}

// Helpers

async fn append_alpha(
    store: &Arc<dyn EventStore>,
    stream: &AggregateId,
    sequence: u64,
    note: &str,
) {
    append_raw(
        store,
        stream,
        sequence,
        Alpha::EVENT_TYPE,
        serde_json::to_vec(&Alpha {
            note: note.to_owned(),
        })
        .map(Bytes::from)
        .expect("encode Alpha must succeed"),
    )
    .await;
}

async fn append_beta(store: &Arc<dyn EventStore>, stream: &AggregateId, sequence: u64, note: &str) {
    append_raw(
        store,
        stream,
        sequence,
        Beta::EVENT_TYPE,
        serde_json::to_vec(&Beta {
            note: note.to_owned(),
        })
        .map(Bytes::from)
        .expect("encode Beta must succeed"),
    )
    .await;
}

async fn append_raw(
    store: &Arc<dyn EventStore>,
    stream: &AggregateId,
    sequence: u64,
    event_type: EventType,
    payload: Bytes,
) {
    store
        .append(
            stream.as_str(),
            vec![AppendingEvent {
                sequence,
                event_type,
                payload,
                occurred_at: jiff::Timestamp::now(),
            }],
        )
        .await
        .expect("appending an upstream record must succeed");
}

const BARRIER_TIMEOUT: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(25);

fn seen(handled: &Handled) -> Vec<Seen> {
    handled
        .lock()
        .expect("handled buffer must not be poisoned")
        .clone()
}

async fn wait_for_handled(handled: &Handled, expected: usize) -> Vec<Seen> {
    let deadline = Instant::now() + BARRIER_TIMEOUT;
    loop {
        let current = seen(handled);
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

async fn wait_for_positions(store: &Arc<dyn EventStore>, saga_id: &SagaId, expected: usize) {
    let deadline = Instant::now() + BARRIER_TIMEOUT;
    loop {
        let rows: Vec<LoadedEvent> = store
            .load(LoadQuery::by_stream(saga_id))
            .await
            .expect("load saga stream must succeed")
            .try_collect()
            .await
            .expect("collect saga events must succeed");
        let positions = rows
            .iter()
            .filter(|row| row.event_type == CONSUMED_POSITION_MARKER)
            .count();
        if positions >= expected {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {expected} consumed-position rows; saw {positions}"
        );
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

fn as_set(observed: &[Seen]) -> Vec<Seen> {
    let mut sorted = observed.to_vec();
    sorted.sort();
    sorted
}

async fn spawn_two_source_saga(
    ps: &ProcessSystem,
    saga_store: &Arc<dyn EventStore>,
    upstream_store: &Arc<dyn EventStore>,
    alpha_stream: &AggregateId,
    beta_stream: &AggregateId,
    handled: &Handled,
) -> nitinol_saga::SagaProxy<TwoSourceSaga> {
    let handled = Arc::clone(handled);
    SagaProps::<TwoSourceSaga>::new(
        SagaId::new(SAGA_ID),
        Arc::clone(saga_store),
        move |_components| TwoSourceSaga {
            handled: Arc::clone(&handled),
        },
    )
    .subscribed_to::<Alpha>(Subscription::stream(upstream_store, alpha_stream))
    .subscribed_to::<Beta>(Subscription::stream(upstream_store, beta_stream))
    .spawn(ps)
    .await
}

// Tests

/// Given a saga that consumed every record of source A and none of source B,
/// When a fresh incarnation subscribes to both sources from their floors again,
/// Then A delivers nothing it already consumed while B delivers from its floor:
/// one position per source, and neither speaks for the other.
#[tokio::test]
async fn each_source_resumes_from_its_own_consumed_position() {
    let ps = ProcessSystem::new().await;

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let alpha_stream = AggregateId::new("icur-alpha");
    let beta_stream = AggregateId::new("icur-beta");
    append_alpha(&upstream_store, &alpha_stream, 1, "a1").await;
    append_alpha(&upstream_store, &alpha_stream, 2, "a2").await;
    append_alpha(&upstream_store, &alpha_stream, 3, "a3").await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new(SAGA_ID);
    let first_handled: Handled = Arc::new(Mutex::new(Vec::new()));

    let first = spawn_two_source_saga(
        &ps,
        &saga_store,
        &upstream_store,
        &alpha_stream,
        &beta_stream,
        &first_handled,
    )
    .await;

    let first_seen = wait_for_handled(&first_handled, 3).await;
    assert_eq!(
        as_set(&first_seen),
        vec![
            Seen {
                variant: "First",
                sequence: 1
            },
            Seen {
                variant: "First",
                sequence: 2
            },
            Seen {
                variant: "First",
                sequence: 3
            },
        ],
        "the first incarnation must consume source A and see nothing of the \
         empty source B; saw {first_seen:?}"
    );
    // Every decision carries its input's position, so three positions means
    // all three of A's records are durably consumed.
    wait_for_positions(&saga_store, &saga_id, 3).await;

    first.stop().await.expect("stop must succeed");
    drop(first);

    // Source B gets its first record only now, and source A gets one more.
    append_beta(&upstream_store, &beta_stream, 1, "b1").await;
    append_alpha(&upstream_store, &alpha_stream, 4, "a4").await;

    let second_handled: Handled = Arc::new(Mutex::new(Vec::new()));
    let _second = spawn_two_source_saga(
        &ps,
        &saga_store,
        &upstream_store,
        &alpha_stream,
        &beta_stream,
        &second_handled,
    )
    .await;

    let second_seen = wait_for_handled(&second_handled, 2).await;
    assert_eq!(
        as_set(&second_seen),
        vec![
            Seen {
                variant: "First",
                sequence: 4
            },
            Seen {
                variant: "Second",
                sequence: 1
            },
        ],
        "source A must resume past the position it recorded for itself, and \
         source B must start at its own floor — a single shared position would \
         either re-deliver A's first three records or cover B's first; saw \
         {second_seen:?}"
    );
}

/// Given a saga one of whose declared sources never yields a record,
/// When a record of the other source arrives,
/// Then it reaches `handle`: a source that is stuck holds back only itself.
#[tokio::test]
async fn a_stalled_source_does_not_stop_the_other_from_progressing() {
    let ps = ProcessSystem::new().await;

    let alpha_stream = AggregateId::new("stall-alpha");
    let beta_stream = AggregateId::new("stall-beta");
    let upstream_store: Arc<dyn EventStore> = Arc::new(StallingStore {
        inner: InMemoryEventStore::default(),
        stalled_key: alpha_stream.as_str().to_owned(),
    });
    // Written through the decorator, which serves appends normally; only reads
    // of the alpha stream stall.
    append_alpha(&upstream_store, &alpha_stream, 1, "never-delivered").await;
    append_beta(&upstream_store, &beta_stream, 1, "b1").await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let handled: Handled = Arc::new(Mutex::new(Vec::new()));

    let _proxy = spawn_two_source_saga(
        &ps,
        &saga_store,
        &upstream_store,
        &alpha_stream,
        &beta_stream,
        &handled,
    )
    .await;

    let observed = wait_for_handled(&handled, 1).await;

    assert_eq!(
        observed,
        vec![Seen {
            variant: "Second",
            sequence: 1
        }],
        "source B must be delivered while source A is stalled; a single \
         subscription over both sources would deliver neither; saw {observed:?}"
    );
}
