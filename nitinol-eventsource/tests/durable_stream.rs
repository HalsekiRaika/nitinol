// Integration tests for `DurableStream<T>` — at-least-once stream with catchup + live.
//
// These tests pin the public contract of the new `nitinol-eventsource::durable_stream`
// module (Issue #41).  They are written before the implementation, so compile / run
// failures are expected until the production code lands.
//
// Public surface exercised:
//   - `DurableStream::<T>::new(topic, store, transform)`
//   - typestate builder: `.cursor(SequenceCursor) -> .with_poll_interval(Duration) -> .spawn(&system)`
//   - `DurableStreamProxy<T>::subscribe(ProcessProxy<P>) where P: Receive<T, Response=()>`
//   - drop-guard: dropping the proxy aborts the polling task.
//
// Note: tests run with a moderate poll interval (50ms) and a 2-second wait budget
// for asynchronous delivery; subscribers are registered immediately after `spawn`.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use tokio::sync::Notify;

use nitinol_eventsource::{DurableStream, Event, EventEnvelope, SequenceCursor};
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::{AggregateId, AppendingEvent, EventType, LoadedEvent};
use nitinol_runtime::ident::ProcessName;
use nitinol_runtime::process::{Process, ProcessContext, Props, Receive};
use nitinol_runtime::ProcessSystem;

// ---------------------------------------------------------------------------
// Fixtures: event type used by the transform
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Evt;

impl Event for Evt {
    const EVENT_TYPE: EventType = EventType::from_str("Evt");
}

/// A second event type used to verify that `transform` returning `None` skips
/// the event without halting delivery of subsequent matching events.
const OTHER_EVENT_TYPE: EventType = EventType::from_str("OtherEvt");

// ---------------------------------------------------------------------------
// Fixtures: transform from LoadedEvent into the durable stream's payload type
// ---------------------------------------------------------------------------

/// `transform` accepts the raw `LoadedEvent` and returns `Some(EventEnvelope<Evt>)`
/// only when the event_type matches.  All non-matching types yield `None` and
/// must be skipped by the poller without halting the loop.
fn to_envelope(loaded: LoadedEvent) -> Option<EventEnvelope<Evt>> {
    if loaded.event_type != Evt::EVENT_TYPE {
        return None;
    }
    Some(EventEnvelope {
        aggregate_id: AggregateId::new(loaded.stream_key),
        sequence: loaded.sequence,
        global_sequence: loaded.global_sequence,
        event: Evt,
    })
}

// ---------------------------------------------------------------------------
// Fixtures: a recording subscriber process
// ---------------------------------------------------------------------------

/// Captures every `EventEnvelope<Evt>` it receives in publish order so the
/// test can assert which events arrived and in what order.
struct RecordingSubscriber {
    sequences: Arc<Mutex<Vec<u64>>>,
    globals: Arc<Mutex<Vec<u64>>>,
    count: Arc<AtomicUsize>,
    notify: Arc<Notify>,
}

impl Process for RecordingSubscriber {}

impl Receive<EventEnvelope<Evt>> for RecordingSubscriber {
    type Response = ();
    type Error = std::convert::Infallible;

    async fn recv(
        &mut self,
        msg: EventEnvelope<Evt>,
        _ctx: &mut ProcessContext,
    ) -> Result<(), Self::Error> {
        self.sequences.lock().unwrap().push(msg.sequence);
        self.globals.lock().unwrap().push(msg.global_sequence);
        self.count.fetch_add(1, Ordering::SeqCst);
        self.notify.notify_one();
        Ok(())
    }
}

/// Aggregates the shared state needed to spawn a `RecordingSubscriber` and
/// inspect what it has seen.
struct Recorder {
    sequences: Arc<Mutex<Vec<u64>>>,
    globals: Arc<Mutex<Vec<u64>>>,
    count: Arc<AtomicUsize>,
    notify: Arc<Notify>,
}

impl Recorder {
    fn new() -> Self {
        Self {
            sequences: Arc::new(Mutex::new(Vec::new())),
            globals: Arc::new(Mutex::new(Vec::new())),
            count: Arc::new(AtomicUsize::new(0)),
            notify: Arc::new(Notify::new()),
        }
    }

    fn producer(&self) -> impl Fn() -> RecordingSubscriber + Send + Sync + 'static {
        let sequences = Arc::clone(&self.sequences);
        let globals = Arc::clone(&self.globals);
        let count = Arc::clone(&self.count);
        let notify = Arc::clone(&self.notify);
        move || RecordingSubscriber {
            sequences: Arc::clone(&sequences),
            globals: Arc::clone(&globals),
            count: Arc::clone(&count),
            notify: Arc::clone(&notify),
        }
    }

    fn seen_sequences(&self) -> Vec<u64> {
        self.sequences.lock().unwrap().clone()
    }

    fn seen_globals(&self) -> Vec<u64> {
        self.globals.lock().unwrap().clone()
    }

    fn current_count(&self) -> usize {
        self.count.load(Ordering::SeqCst)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn append_evt(store: &InMemoryEventStore, key: &str, sequence: u64) {
    append_with_type(store, key, sequence, Evt::EVENT_TYPE).await
}

async fn append_with_type(
    store: &InMemoryEventStore,
    key: &str,
    sequence: u64,
    event_type: EventType,
) {
    store
        .append(
            key,
            vec![AppendingEvent {
                sequence,
                event_type,
                payload: Bytes::new(),
                occurred_at: jiff::Timestamp::now(),
            }],
        )
        .await
        .expect("append must succeed");
}

async fn wait_for_count(recorder: &Recorder, expected: usize) {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let notified = recorder.notify.notified();
            if recorder.current_count() >= expected {
                return;
            }
            notified.await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "timed out waiting for {expected} envelopes (received {})",
            recorder.current_count()
        )
    });
}

const TEST_POLL_INTERVAL: Duration = Duration::from_millis(50);

// ---------------------------------------------------------------------------
// Test: catch up all pre-existing events from an empty cursor
// ---------------------------------------------------------------------------

/// Given three events appended before `spawn`, when a DurableStream starts with
/// `SequenceCursor::Stream { after: 0 }`, then the subscriber receives all
/// three events in sequence order.
#[tokio::test]
async fn durable_stream_catchup_all_events_from_zero() {
    // Given: three pre-existing events
    let system = ProcessSystem::new().await;
    let store = Arc::new(InMemoryEventStore::default());
    let agg_id = AggregateId::new("ds-catchup-agg");
    for seq in 1..=3 {
        append_evt(&store, agg_id.as_str(), seq).await;
    }

    let recorder = Recorder::new();
    let sub_proxy = system.spawn(Props::new(recorder.producer())).await;

    // When: spawn the durable stream from the very beginning of the stream
    let ds = DurableStream::<EventEnvelope<Evt>>::new(
        ProcessName::new("ds-catchup-topic"),
        Arc::clone(&store) as Arc<dyn EventStore>,
        to_envelope,
    )
    .cursor(SequenceCursor::Stream {
        key: agg_id.as_str().to_owned(),
        after: 0,
    })
    .with_poll_interval(TEST_POLL_INTERVAL)
    .spawn(&system)
    .await
    .expect("DurableStream::spawn must succeed");

    ds.subscribe(sub_proxy)
        .await
        .expect("subscribe must succeed");

    // Then: all three pre-existing events arrive in ascending sequence order
    wait_for_count(&recorder, 3).await;
    assert_eq!(
        recorder.seen_sequences(),
        vec![1, 2, 3],
        "all pre-existing events must be replayed during catch-up"
    );
}

// ---------------------------------------------------------------------------
// Test: resume from a non-zero cursor — only later events are delivered
// ---------------------------------------------------------------------------

/// Given three events at sequence 1, 2, 3, when a DurableStream starts with
/// `SequenceCursor::Stream { after: 2 }`, then only sequence=3 is delivered.
#[tokio::test]
async fn durable_stream_resumes_from_start_sequence() {
    // Given
    let system = ProcessSystem::new().await;
    let store = Arc::new(InMemoryEventStore::default());
    let agg_id = AggregateId::new("ds-resume-agg");
    for seq in 1..=3 {
        append_evt(&store, agg_id.as_str(), seq).await;
    }

    let recorder = Recorder::new();
    let sub_proxy = system.spawn(Props::new(recorder.producer())).await;

    // When: start past sequence=2 (i.e. resume after a checkpoint at 2)
    let ds = DurableStream::<EventEnvelope<Evt>>::new(
        ProcessName::new("ds-resume-topic"),
        Arc::clone(&store) as Arc<dyn EventStore>,
        to_envelope,
    )
    .cursor(SequenceCursor::Stream {
        key: agg_id.as_str().to_owned(),
        after: 2,
    })
    .with_poll_interval(TEST_POLL_INTERVAL)
    .spawn(&system)
    .await
    .expect("DurableStream::spawn must succeed");

    ds.subscribe(sub_proxy)
        .await
        .expect("subscribe must succeed");

    // Then: only the event past the cursor (seq=3) is delivered
    wait_for_count(&recorder, 1).await;
    // Give the poller one more interval to make sure no extras leak through.
    tokio::time::sleep(TEST_POLL_INTERVAL * 2).await;
    assert_eq!(
        recorder.seen_sequences(),
        vec![3],
        "only events after the cursor's `after` value must be delivered"
    );
}

// ---------------------------------------------------------------------------
// Test: live events appended after spawn are picked up by the poller
// ---------------------------------------------------------------------------

/// Given a DurableStream that has finished catching up an empty store, when
/// new events are appended, then the subscriber receives them within the
/// configured poll interval.
#[tokio::test]
async fn durable_stream_live_events_after_catchup() {
    // Given: empty store and a subscribed durable stream
    let system = ProcessSystem::new().await;
    let store = Arc::new(InMemoryEventStore::default());
    let agg_id = AggregateId::new("ds-live-agg");

    let recorder = Recorder::new();
    let sub_proxy = system.spawn(Props::new(recorder.producer())).await;

    let ds = DurableStream::<EventEnvelope<Evt>>::new(
        ProcessName::new("ds-live-topic"),
        Arc::clone(&store) as Arc<dyn EventStore>,
        to_envelope,
    )
    .cursor(SequenceCursor::Stream {
        key: agg_id.as_str().to_owned(),
        after: 0,
    })
    .with_poll_interval(TEST_POLL_INTERVAL)
    .spawn(&system)
    .await
    .expect("DurableStream::spawn must succeed");

    ds.subscribe(sub_proxy)
        .await
        .expect("subscribe must succeed");

    // When: append two events after spawn
    append_evt(&store, agg_id.as_str(), 1).await;
    append_evt(&store, agg_id.as_str(), 2).await;

    // Then: subscriber receives both via polling
    wait_for_count(&recorder, 2).await;
    assert_eq!(
        recorder.seen_sequences(),
        vec![1, 2],
        "live events appended after spawn must be delivered in order"
    );
}

// ---------------------------------------------------------------------------
// Test: multiple subscribers — each receives every event
// ---------------------------------------------------------------------------

/// Given two subscribers attached to the same DurableStream, when events are
/// produced, then both subscribers receive every event independently
/// (fan-out semantics inherited from the underlying `Stream<T>`).
#[tokio::test]
async fn durable_stream_multiple_subscribers_each_receive() {
    // Given
    let system = ProcessSystem::new().await;
    let store = Arc::new(InMemoryEventStore::default());
    let agg_id = AggregateId::new("ds-multi-agg");

    let recorder_a = Recorder::new();
    let recorder_b = Recorder::new();
    let sub_a = system.spawn(Props::new(recorder_a.producer())).await;
    let sub_b = system.spawn(Props::new(recorder_b.producer())).await;

    let ds = DurableStream::<EventEnvelope<Evt>>::new(
        ProcessName::new("ds-multi-topic"),
        Arc::clone(&store) as Arc<dyn EventStore>,
        to_envelope,
    )
    .cursor(SequenceCursor::Stream {
        key: agg_id.as_str().to_owned(),
        after: 0,
    })
    .with_poll_interval(TEST_POLL_INTERVAL)
    .spawn(&system)
    .await
    .expect("DurableStream::spawn must succeed");

    ds.subscribe(sub_a).await.expect("subscribe A must succeed");
    ds.subscribe(sub_b).await.expect("subscribe B must succeed");

    // When: produce two events
    append_evt(&store, agg_id.as_str(), 1).await;
    append_evt(&store, agg_id.as_str(), 2).await;

    // Then: each subscriber sees both events
    wait_for_count(&recorder_a, 2).await;
    wait_for_count(&recorder_b, 2).await;
    assert_eq!(
        recorder_a.seen_sequences(),
        vec![1, 2],
        "subscriber A must receive every event"
    );
    assert_eq!(
        recorder_b.seen_sequences(),
        vec![1, 2],
        "subscriber B must receive every event"
    );
}

// ---------------------------------------------------------------------------
// Test: dropping the proxy stops the poller
// ---------------------------------------------------------------------------

/// Given a DurableStreamProxy with one subscribed observer, when the proxy is
/// dropped, then any events appended afterwards must not reach the subscriber
/// (the poller task is aborted by the proxy's drop-guard).
#[tokio::test]
async fn durable_stream_drop_proxy_stops_poller() {
    // Given: a stream catching up an empty store
    let system = ProcessSystem::new().await;
    let store = Arc::new(InMemoryEventStore::default());
    let agg_id = AggregateId::new("ds-drop-agg");

    let recorder = Recorder::new();
    let sub_proxy = system.spawn(Props::new(recorder.producer())).await;

    let ds = DurableStream::<EventEnvelope<Evt>>::new(
        ProcessName::new("ds-drop-topic"),
        Arc::clone(&store) as Arc<dyn EventStore>,
        to_envelope,
    )
    .cursor(SequenceCursor::Stream {
        key: agg_id.as_str().to_owned(),
        after: 0,
    })
    .with_poll_interval(TEST_POLL_INTERVAL)
    .spawn(&system)
    .await
    .expect("DurableStream::spawn must succeed");

    ds.subscribe(sub_proxy)
        .await
        .expect("subscribe must succeed");

    // First event arrives normally
    append_evt(&store, agg_id.as_str(), 1).await;
    wait_for_count(&recorder, 1).await;
    let baseline = recorder.current_count();

    // When: drop the proxy, then append more events
    drop(ds);
    // Give the runtime a moment to actually abort the polling task.
    tokio::time::sleep(TEST_POLL_INTERVAL * 4).await;

    append_evt(&store, agg_id.as_str(), 2).await;
    append_evt(&store, agg_id.as_str(), 3).await;

    // Then: wait long enough for several polls — the subscriber must not receive
    // any further events.
    tokio::time::sleep(TEST_POLL_INTERVAL * 6).await;
    assert_eq!(
        recorder.current_count(),
        baseline,
        "no further events must be delivered once the DurableStreamProxy is dropped"
    );
}

// ---------------------------------------------------------------------------
// Test: transform returning None skips the event but does not halt the stream
// ---------------------------------------------------------------------------

/// Given a mix of matching and non-matching event types, when the transform
/// returns `None` for non-matching ones, then only the matching events reach
/// the subscriber and the poller continues to advance past skipped events.
#[tokio::test]
async fn durable_stream_transform_returns_none_skips_event() {
    // Given: an aggregate with two `Evt` events sandwiching a non-matching event.
    // Layout (per-stream sequences):
    //   seq=1: Evt        → must be delivered
    //   seq=2: OtherEvt   → transform returns None → must be skipped
    //   seq=3: Evt        → must be delivered after the skipped one
    let system = ProcessSystem::new().await;
    let store = Arc::new(InMemoryEventStore::default());
    let agg_id = AggregateId::new("ds-skip-agg");

    append_with_type(&store, agg_id.as_str(), 1, Evt::EVENT_TYPE).await;
    append_with_type(&store, agg_id.as_str(), 2, OTHER_EVENT_TYPE).await;
    append_with_type(&store, agg_id.as_str(), 3, Evt::EVENT_TYPE).await;

    let recorder = Recorder::new();
    let sub_proxy = system.spawn(Props::new(recorder.producer())).await;

    // When
    let ds = DurableStream::<EventEnvelope<Evt>>::new(
        ProcessName::new("ds-skip-topic"),
        Arc::clone(&store) as Arc<dyn EventStore>,
        to_envelope,
    )
    .cursor(SequenceCursor::Stream {
        key: agg_id.as_str().to_owned(),
        after: 0,
    })
    .with_poll_interval(TEST_POLL_INTERVAL)
    .spawn(&system)
    .await
    .expect("DurableStream::spawn must succeed");

    ds.subscribe(sub_proxy)
        .await
        .expect("subscribe must succeed");

    // Then: only the matching events arrive, and the skipped middle event does
    // not block delivery of the later matching event.
    wait_for_count(&recorder, 2).await;
    tokio::time::sleep(TEST_POLL_INTERVAL * 2).await;
    assert_eq!(
        recorder.seen_sequences(),
        vec![1, 3],
        "transform returning None must skip events without stalling the stream"
    );
}

// ---------------------------------------------------------------------------
// Test: SequenceCursor::Global orders events across aggregates
// ---------------------------------------------------------------------------

/// Given two aggregates with interleaved appends, when a DurableStream is
/// started with `SequenceCursor::Global { after: 0 }`, then events arrive in
/// ascending global_sequence order regardless of which aggregate produced them.
#[tokio::test]
async fn durable_stream_global_cursor_orders_across_aggregates() {
    // Given: interleaved appends from two aggregates
    let system = ProcessSystem::new().await;
    let store = Arc::new(InMemoryEventStore::default());
    let agg_a = AggregateId::new("ds-global-a");
    let agg_b = AggregateId::new("ds-global-b");

    // Global sequence is assigned monotonically by append order:
    // A-seq1 → global=1, B-seq1 → global=2, A-seq2 → global=3, B-seq2 → global=4.
    append_evt(&store, agg_a.as_str(), 1).await;
    append_evt(&store, agg_b.as_str(), 1).await;
    append_evt(&store, agg_a.as_str(), 2).await;
    append_evt(&store, agg_b.as_str(), 2).await;

    let recorder = Recorder::new();
    let sub_proxy = system.spawn(Props::new(recorder.producer())).await;

    // When: subscribe via the global cursor
    let ds = DurableStream::<EventEnvelope<Evt>>::new(
        ProcessName::new("ds-global-topic"),
        Arc::clone(&store) as Arc<dyn EventStore>,
        to_envelope,
    )
    .cursor(SequenceCursor::Global { after: 0 })
    .with_poll_interval(TEST_POLL_INTERVAL)
    .spawn(&system)
    .await
    .expect("DurableStream::spawn must succeed");

    ds.subscribe(sub_proxy)
        .await
        .expect("subscribe must succeed");

    // Then: all four events arrive ordered by global_sequence
    wait_for_count(&recorder, 4).await;
    assert_eq!(
        recorder.seen_globals(),
        vec![1, 2, 3, 4],
        "Global cursor must deliver events in ascending global_sequence order"
    );
}

// ---------------------------------------------------------------------------
// Regression test: AI-DS-001 — catchup must not be lost before subscribe
// ---------------------------------------------------------------------------

/// Regression for AI-DS-001: the poller must not advance the cursor before the
/// first subscriber registers, even under multi-threaded scheduling where the
/// poller could win the race.
///
/// Without the oneshot-gate fix the poller would poll immediately after
/// `spawn()`, publish to a subscriber-less `Stream<T>`, advance the cursor, and
/// drop all catchup events.  The test makes that race deterministic by sleeping
/// for several poll intervals between `spawn` and `subscribe`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn durable_stream_catchup_not_lost_before_subscribe() {
    // Given: three pre-existing events
    let system = ProcessSystem::new().await;
    let store = Arc::new(InMemoryEventStore::default());
    let agg_id = AggregateId::new("ds-presubscribe-agg");
    for seq in 1..=3 {
        append_evt(&store, agg_id.as_str(), seq).await;
    }

    let recorder = Recorder::new();
    let sub_proxy = system.spawn(Props::new(recorder.producer())).await;

    // When: spawn the durable stream but deliberately delay subscribing by
    // multiple poll intervals.  Without the fix the poller would advance the
    // cursor during this window and the subscriber would receive nothing.
    let ds = DurableStream::<EventEnvelope<Evt>>::new(
        ProcessName::new("ds-presubscribe-topic"),
        Arc::clone(&store) as Arc<dyn EventStore>,
        to_envelope,
    )
    .cursor(SequenceCursor::Stream {
        key: agg_id.as_str().to_owned(),
        after: 0,
    })
    .with_poll_interval(TEST_POLL_INTERVAL)
    .spawn(&system)
    .await
    .expect("DurableStream::spawn must succeed");

    // Deliberate delay: give the poller multiple opportunities to poll before
    // the subscriber is registered.
    tokio::time::sleep(TEST_POLL_INTERVAL * 5).await;

    ds.subscribe(sub_proxy)
        .await
        .expect("subscribe must succeed");

    // Then: all three pre-existing events must still arrive despite the delayed
    // subscription.
    wait_for_count(&recorder, 3).await;
    assert_eq!(
        recorder.seen_sequences(),
        vec![1, 2, 3],
        "catchup events must not be lost even when subscribe is delayed after spawn"
    );
}

// ---------------------------------------------------------------------------
// Regression test: AI-DS-002 — late subscriber catches up from its own cursor
// ---------------------------------------------------------------------------

/// Regression for AI-DS-002: a subscriber that registers after the shared
/// poller cursor has already advanced must still receive all historical events
/// from its own checkpoint via `subscribe_from`.
///
/// Without per-subscriber catchup, the shared cursor is at seq=3 when the
/// second subscriber registers with `after: 0`, and no historical events
/// would be delivered to it.
#[tokio::test]
async fn durable_stream_late_subscriber_catchup_from_own_cursor() {
    // Given: three pre-existing events
    let system = ProcessSystem::new().await;
    let store = Arc::new(InMemoryEventStore::default());
    let agg_id = AggregateId::new("ds-late-sub-agg");
    for seq in 1..=3 {
        append_evt(&store, agg_id.as_str(), seq).await;
    }

    // Spawn the durable stream from the beginning
    let ds = DurableStream::<EventEnvelope<Evt>>::new(
        ProcessName::new("ds-late-sub-topic"),
        Arc::clone(&store) as Arc<dyn EventStore>,
        to_envelope,
    )
    .cursor(SequenceCursor::Stream {
        key: agg_id.as_str().to_owned(),
        after: 0,
    })
    .with_poll_interval(TEST_POLL_INTERVAL)
    .spawn(&system)
    .await
    .expect("DurableStream::spawn must succeed");

    // First subscriber: subscribe and wait for its catchup to complete so that
    // the shared poller cursor has advanced to seq=3.
    let recorder_a = Recorder::new();
    let sub_a = system.spawn(Props::new(recorder_a.producer())).await;
    ds.subscribe(sub_a).await.expect("subscribe A must succeed");
    wait_for_count(&recorder_a, 3).await; // cursor now at seq=3

    // Second subscriber: registers AFTER the shared cursor has advanced.
    // It provides its own checkpoint (after: 0) via subscribe_from so that
    // all three historical events are delivered directly to it.
    let recorder_b = Recorder::new();
    let sub_b = system.spawn(Props::new(recorder_b.producer())).await;
    ds.subscribe_from(
        sub_b,
        SequenceCursor::Stream {
            key: agg_id.as_str().to_owned(),
            after: 0,
        },
    )
    .await
    .expect("subscribe_from B must succeed");

    // Then: B must receive all three historical events despite joining late.
    wait_for_count(&recorder_b, 3).await;
    // Allow extra poll intervals to confirm no missing or extraneous events.
    tokio::time::sleep(TEST_POLL_INTERVAL * 2).await;
    assert_eq!(
        recorder_b.seen_sequences(),
        vec![1, 2, 3],
        "late subscriber must receive all events from its own checkpoint even \
         after the shared poller cursor has advanced past them"
    );
}

// ---------------------------------------------------------------------------
// Regression test: AI-DS-003 — subscribe_from must deliver events in
// sequence order even when live events are appended concurrently
// ---------------------------------------------------------------------------

/// Regression for AI-DS-003: `subscribe_from` must guarantee that catchup
/// events arrive at the subscriber before live events, even when live events
/// are present in the event store at the time of subscription.
///
/// The previous implementation registered the subscriber on the shared
/// `Stream<T>` live fan-out first, then spawned a background catchup task.
/// This meant the shared poller could deliver live event seq=4 to the
/// subscriber before the catchup task delivered historical events seq=1,2,3,
/// producing an out-of-order sequence [4, 1, 2, 3].
///
/// With the per-subscriber poller fix, both historical and live events are
/// delivered through the same event-store polling loop in ascending sequence
/// order, so the subscriber always observes [1, 2, 3, 4].
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn durable_stream_subscribe_from_ordering_with_concurrent_live_events() {
    // Given: 3 pre-existing events
    let system = ProcessSystem::new().await;
    let store = Arc::new(InMemoryEventStore::default());
    let agg_id = AggregateId::new("ds-order-agg");
    for seq in 1..=3 {
        append_evt(&store, agg_id.as_str(), seq).await;
    }

    let ds = DurableStream::<EventEnvelope<Evt>>::new(
        ProcessName::new("ds-order-topic"),
        Arc::clone(&store) as Arc<dyn EventStore>,
        to_envelope,
    )
    .cursor(SequenceCursor::Stream {
        key: agg_id.as_str().to_owned(),
        after: 0,
    })
    .with_poll_interval(TEST_POLL_INTERVAL)
    .spawn(&system)
    .await
    .expect("DurableStream::spawn must succeed");

    // First subscriber: release the gate and advance the shared cursor to seq=3.
    let recorder_a = Recorder::new();
    let sub_a = system.spawn(Props::new(recorder_a.producer())).await;
    ds.subscribe(sub_a).await.expect("subscribe A must succeed");
    wait_for_count(&recorder_a, 3).await;

    // Append a live event (seq=4) before calling subscribe_from.
    // With the old implementation the shared poller would deliver seq=4 to
    // sub_b before the catchup task delivered seq=1,2,3 — violating ordering.
    append_evt(&store, agg_id.as_str(), 4).await;

    // Late subscriber: subscribe from the very beginning.
    let recorder_b = Recorder::new();
    let sub_b = system.spawn(Props::new(recorder_b.producer())).await;
    ds.subscribe_from(
        sub_b,
        SequenceCursor::Stream {
            key: agg_id.as_str().to_owned(),
            after: 0,
        },
    )
    .await
    .expect("subscribe_from B must succeed");

    // Then: subscriber B must receive all four events in ascending sequence order.
    wait_for_count(&recorder_b, 4).await;
    tokio::time::sleep(TEST_POLL_INTERVAL * 2).await;
    assert_eq!(
        recorder_b.seen_sequences(),
        vec![1, 2, 3, 4],
        "subscribe_from must deliver events in checkpoint sequence order; \
         catchup events must not be interleaved with live events (AI-DS-003)"
    );
}

// ---------------------------------------------------------------------------
// Test: duplicate topic name causes spawn to fail with SpawnError
// ---------------------------------------------------------------------------

/// Given a `ProcessSystem` already hosting a process under topic name `T`,
/// when a DurableStream is spawned reusing the same topic name, then spawn
/// must return `Err(SpawnError)` — propagated from `system.spawn_stream`.
#[tokio::test]
async fn durable_stream_spawn_with_duplicate_topic_returns_error() {
    // Given: an existing stream registered under "ds-dup-topic"
    let system = ProcessSystem::new().await;
    let store = Arc::new(InMemoryEventStore::default());
    let _existing = system
        .spawn_stream::<EventEnvelope<Evt>>(ProcessName::new("ds-dup-topic"))
        .await
        .expect("initial spawn_stream must succeed");

    // When: try to spawn a DurableStream with the same topic name
    let result = DurableStream::<EventEnvelope<Evt>>::new(
        ProcessName::new("ds-dup-topic"),
        Arc::clone(&store) as Arc<dyn EventStore>,
        to_envelope,
    )
    .cursor(SequenceCursor::Global { after: 0 })
    .with_poll_interval(TEST_POLL_INTERVAL)
    .spawn(&system)
    .await;

    // Then: spawn must return SpawnError (duplicate topic), not panic
    assert!(
        result.is_err(),
        "DurableStream::spawn must propagate SpawnError when the topic is already in use"
    );
}

// ---------------------------------------------------------------------------
// Regression test: AI-DS-005 — subscribe_from must not open the shared
// poller gate; a later subscribe() must still deliver all events
// ---------------------------------------------------------------------------

/// Regression for AI-DS-005: calling `subscribe_from` before any `subscribe`
/// must not open the shared poller's start gate.  When `subscribe` is called
/// later, the shared poller must start from the original cursor and deliver
/// all events to the shared subscriber.
///
/// Without the fix, `subscribe_from` would fire `start_tx` and advance the
/// shared cursor during the interval before `subscribe` is called.  The
/// subsequent `subscribe` subscriber would then see an empty (already-advanced)
/// cursor and receive no historical events.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn durable_stream_subscribe_from_does_not_open_shared_gate() {
    // Given: three pre-existing events
    let system = ProcessSystem::new().await;
    let store = Arc::new(InMemoryEventStore::default());
    let agg_id = AggregateId::new("ds-ds005-agg");
    for seq in 1..=3 {
        append_evt(&store, agg_id.as_str(), seq).await;
    }

    let ds = DurableStream::<EventEnvelope<Evt>>::new(
        ProcessName::new("ds-ds005-topic"),
        Arc::clone(&store) as Arc<dyn EventStore>,
        to_envelope,
    )
    .cursor(SequenceCursor::Stream {
        key: agg_id.as_str().to_owned(),
        after: 0,
    })
    .with_poll_interval(TEST_POLL_INTERVAL)
    .spawn(&system)
    .await
    .expect("DurableStream::spawn must succeed");

    // First: subscribe_from only — no shared subscriber yet.
    let recorder_direct = Recorder::new();
    let sub_direct = system.spawn(Props::new(recorder_direct.producer())).await;
    ds.subscribe_from(
        sub_direct,
        SequenceCursor::Stream {
            key: agg_id.as_str().to_owned(),
            after: 0,
        },
    )
    .await
    .expect("subscribe_from must succeed");

    // Wait for the direct subscriber to receive all three events and then sit
    // idle for multiple poll intervals.  The shared poller must NOT have polled
    // during this window (gate is still closed).
    wait_for_count(&recorder_direct, 3).await;
    tokio::time::sleep(TEST_POLL_INTERVAL * 5).await;

    // Then: register a shared subscriber via subscribe().  This opens the gate.
    let recorder_shared = Recorder::new();
    let sub_shared = system.spawn(Props::new(recorder_shared.producer())).await;
    ds.subscribe(sub_shared)
        .await
        .expect("subscribe must succeed");

    // The shared subscriber must receive all three events because the shared
    // cursor was never advanced while only subscribe_from was in use.
    wait_for_count(&recorder_shared, 3).await;
    tokio::time::sleep(TEST_POLL_INTERVAL * 2).await;
    assert_eq!(
        recorder_shared.seen_sequences(),
        vec![1, 2, 3],
        "shared subscriber must receive all events when subscribe() is called after \
         subscribe_from(); the shared cursor must not have advanced before subscribe() (AI-DS-005)"
    );
}
