// Integration tests for `DurableStream<T>` — at-least-once stream with catchup + live.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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

#[derive(Clone)]
struct Evt;

impl Event for Evt {
    const EVENT_TYPE: EventType = EventType::from_str("Evt");
}

const OTHER_EVENT_TYPE: EventType = EventType::from_str("OtherEvt");

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
        _ctx: &mut ProcessContext<Self>,
    ) -> Result<(), Self::Error> {
        self.sequences.lock().unwrap().push(msg.sequence);
        self.globals.lock().unwrap().push(msg.global_sequence);
        self.count.fetch_add(1, Ordering::SeqCst);
        self.notify.notify_one();
        Ok(())
    }
}

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

#[tokio::test]
async fn durable_stream_catchup_all_events_from_zero() {
    let system = ProcessSystem::new().await;
    let store = Arc::new(InMemoryEventStore::default());
    let agg_id = AggregateId::new("ds-catchup-agg");
    for seq in 1..=3 {
        append_evt(&store, agg_id.as_str(), seq).await;
    }

    let recorder = Recorder::new();
    let sub_proxy = system.spawn(Props::new(recorder.producer())).await;

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

    wait_for_count(&recorder, 3).await;
    assert_eq!(
        recorder.seen_sequences(),
        vec![1, 2, 3],
        "all pre-existing events must be replayed during catch-up"
    );
}

#[tokio::test]
async fn durable_stream_resumes_from_start_sequence() {
    let system = ProcessSystem::new().await;
    let store = Arc::new(InMemoryEventStore::default());
    let agg_id = AggregateId::new("ds-resume-agg");
    for seq in 1..=3 {
        append_evt(&store, agg_id.as_str(), seq).await;
    }

    let recorder = Recorder::new();
    let sub_proxy = system.spawn(Props::new(recorder.producer())).await;

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

    wait_for_count(&recorder, 1).await;
    tokio::time::sleep(TEST_POLL_INTERVAL * 2).await;
    assert_eq!(
        recorder.seen_sequences(),
        vec![3],
        "only events after the cursor's `after` value must be delivered"
    );
}

#[tokio::test]
async fn durable_stream_live_events_after_catchup() {
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

    append_evt(&store, agg_id.as_str(), 1).await;
    append_evt(&store, agg_id.as_str(), 2).await;

    wait_for_count(&recorder, 2).await;
    assert_eq!(
        recorder.seen_sequences(),
        vec![1, 2],
        "live events appended after spawn must be delivered in order"
    );
}

#[tokio::test]
async fn durable_stream_multiple_subscribers_each_receive() {
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

    append_evt(&store, agg_id.as_str(), 1).await;
    append_evt(&store, agg_id.as_str(), 2).await;

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

#[tokio::test]
async fn durable_stream_drop_proxy_stops_poller() {
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

    append_evt(&store, agg_id.as_str(), 1).await;
    wait_for_count(&recorder, 1).await;
    let baseline = recorder.current_count();

    drop(ds);
    tokio::time::sleep(TEST_POLL_INTERVAL * 4).await;

    append_evt(&store, agg_id.as_str(), 2).await;
    append_evt(&store, agg_id.as_str(), 3).await;

    tokio::time::sleep(TEST_POLL_INTERVAL * 6).await;
    assert_eq!(
        recorder.current_count(),
        baseline,
        "no further events must be delivered once the DurableStreamProxy is dropped"
    );
}

#[tokio::test]
async fn durable_stream_transform_returns_none_skips_event() {
    let system = ProcessSystem::new().await;
    let store = Arc::new(InMemoryEventStore::default());
    let agg_id = AggregateId::new("ds-skip-agg");

    append_with_type(&store, agg_id.as_str(), 1, Evt::EVENT_TYPE).await;
    append_with_type(&store, agg_id.as_str(), 2, OTHER_EVENT_TYPE).await;
    append_with_type(&store, agg_id.as_str(), 3, Evt::EVENT_TYPE).await;

    let recorder = Recorder::new();
    let sub_proxy = system.spawn(Props::new(recorder.producer())).await;

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

    wait_for_count(&recorder, 2).await;
    tokio::time::sleep(TEST_POLL_INTERVAL * 2).await;
    assert_eq!(
        recorder.seen_sequences(),
        vec![1, 3],
        "transform returning None must skip events without stalling the stream"
    );
}

#[tokio::test]
async fn durable_stream_global_cursor_orders_across_aggregates() {
    let system = ProcessSystem::new().await;
    let store = Arc::new(InMemoryEventStore::default());
    let agg_a = AggregateId::new("ds-global-a");
    let agg_b = AggregateId::new("ds-global-b");

    append_evt(&store, agg_a.as_str(), 1).await;
    append_evt(&store, agg_b.as_str(), 1).await;
    append_evt(&store, agg_a.as_str(), 2).await;
    append_evt(&store, agg_b.as_str(), 2).await;

    let recorder = Recorder::new();
    let sub_proxy = system.spawn(Props::new(recorder.producer())).await;

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

    wait_for_count(&recorder, 4).await;
    assert_eq!(
        recorder.seen_globals(),
        vec![1, 2, 3, 4],
        "Global cursor must deliver events in ascending global_sequence order"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn durable_stream_catchup_not_lost_before_subscribe() {
    let system = ProcessSystem::new().await;
    let store = Arc::new(InMemoryEventStore::default());
    let agg_id = AggregateId::new("ds-presubscribe-agg");
    for seq in 1..=3 {
        append_evt(&store, agg_id.as_str(), seq).await;
    }

    let recorder = Recorder::new();
    let sub_proxy = system.spawn(Props::new(recorder.producer())).await;

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

    tokio::time::sleep(TEST_POLL_INTERVAL * 5).await;

    ds.subscribe(sub_proxy)
        .await
        .expect("subscribe must succeed");

    wait_for_count(&recorder, 3).await;
    assert_eq!(
        recorder.seen_sequences(),
        vec![1, 2, 3],
        "catchup events must not be lost even when subscribe is delayed after spawn"
    );
}

#[tokio::test]
async fn durable_stream_late_subscriber_catchup_from_own_cursor() {
    let system = ProcessSystem::new().await;
    let store = Arc::new(InMemoryEventStore::default());
    let agg_id = AggregateId::new("ds-late-sub-agg");
    for seq in 1..=3 {
        append_evt(&store, agg_id.as_str(), seq).await;
    }

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

    let recorder_a = Recorder::new();
    let sub_a = system.spawn(Props::new(recorder_a.producer())).await;
    ds.subscribe(sub_a).await.expect("subscribe A must succeed");
    wait_for_count(&recorder_a, 3).await;

    let recorder_b = Recorder::new();
    let sub_b = system.spawn(Props::new(recorder_b.producer())).await;
    ds.subscribe_from(
        &system,
        sub_b,
        SequenceCursor::Stream {
            key: agg_id.as_str().to_owned(),
            after: 0,
        },
    )
    .await
    .expect("subscribe_from B must succeed");

    wait_for_count(&recorder_b, 3).await;
    tokio::time::sleep(TEST_POLL_INTERVAL * 2).await;
    assert_eq!(
        recorder_b.seen_sequences(),
        vec![1, 2, 3],
        "late subscriber must receive all events from its own checkpoint even \
         after the shared poller cursor has advanced past them"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn durable_stream_subscribe_from_ordering_with_concurrent_live_events() {
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

    let recorder_a = Recorder::new();
    let sub_a = system.spawn(Props::new(recorder_a.producer())).await;
    ds.subscribe(sub_a).await.expect("subscribe A must succeed");
    wait_for_count(&recorder_a, 3).await;

    append_evt(&store, agg_id.as_str(), 4).await;

    let recorder_b = Recorder::new();
    let sub_b = system.spawn(Props::new(recorder_b.producer())).await;
    ds.subscribe_from(
        &system,
        sub_b,
        SequenceCursor::Stream {
            key: agg_id.as_str().to_owned(),
            after: 0,
        },
    )
    .await
    .expect("subscribe_from B must succeed");

    wait_for_count(&recorder_b, 4).await;
    tokio::time::sleep(TEST_POLL_INTERVAL * 2).await;
    assert_eq!(
        recorder_b.seen_sequences(),
        vec![1, 2, 3, 4],
        "subscribe_from must deliver events in checkpoint sequence order; \
         catchup events must not be interleaved with live events (AI-DS-003)"
    );
}

#[tokio::test]
async fn durable_stream_spawn_with_duplicate_topic_returns_error() {
    let system = ProcessSystem::new().await;
    let store = Arc::new(InMemoryEventStore::default());
    let _existing = system
        .spawn(nitinol_runtime::StreamProps::<EventEnvelope<Evt>>::new(
            ProcessName::new("ds-dup-topic"),
        ))
        .await
        .expect("initial spawn must succeed");

    let result = DurableStream::<EventEnvelope<Evt>>::new(
        ProcessName::new("ds-dup-topic"),
        Arc::clone(&store) as Arc<dyn EventStore>,
        to_envelope,
    )
    .cursor(SequenceCursor::Global { after: 0 })
    .with_poll_interval(TEST_POLL_INTERVAL)
    .spawn(&system)
    .await;

    assert!(
        result.is_err(),
        "DurableStream::spawn must propagate SpawnError when the topic is already in use"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn durable_stream_subscribe_from_does_not_open_shared_gate() {
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

    let recorder_direct = Recorder::new();
    let sub_direct = system.spawn(Props::new(recorder_direct.producer())).await;
    ds.subscribe_from(
        &system,
        sub_direct,
        SequenceCursor::Stream {
            key: agg_id.as_str().to_owned(),
            after: 0,
        },
    )
    .await
    .expect("subscribe_from must succeed");

    wait_for_count(&recorder_direct, 3).await;
    tokio::time::sleep(TEST_POLL_INTERVAL * 5).await;

    let recorder_shared = Recorder::new();
    let sub_shared = system.spawn(Props::new(recorder_shared.producer())).await;
    ds.subscribe(sub_shared)
        .await
        .expect("subscribe must succeed");

    wait_for_count(&recorder_shared, 3).await;
    tokio::time::sleep(TEST_POLL_INTERVAL * 2).await;
    assert_eq!(
        recorder_shared.seen_sequences(),
        vec![1, 2, 3],
        "shared subscriber must receive all events when subscribe() is called after subscribe_from(); the shared cursor must not have advanced before subscribe()"
    );
}

#[tokio::test]
async fn durable_stream_shared_poller_observable_via_registry_lookup_by_name() {
    let system = ProcessSystem::new().await;
    let store = Arc::new(InMemoryEventStore::default());

    let ds = DurableStream::<EventEnvelope<Evt>>::new(
        ProcessName::new("ds-registry-topic"),
        Arc::clone(&store) as Arc<dyn EventStore>,
        to_envelope,
    )
    .cursor(SequenceCursor::Global { after: 0 })
    .with_poll_interval(TEST_POLL_INTERVAL)
    .spawn(&system)
    .await
    .expect("DurableStream::spawn must succeed");

    let poller_name = ProcessName::new(format!("durable-poller-{}", ds.pid()));
    let found = system.lookup_by_name(&poller_name).await;

    assert!(
        found.is_some(),
        "shared DurablePoller must be registered in the runtime registry under \
         '{poller_name}'"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn durable_stream_poller_panic_is_recovered_by_supervision_restart() {
    let system = ProcessSystem::new().await;
    let store = Arc::new(InMemoryEventStore::default());
    let agg_id = AggregateId::new("ds-panic-agg");
    for seq in 1..=3 {
        append_evt(&store, agg_id.as_str(), seq).await;
    }

    let panic_armed = Arc::new(AtomicBool::new(true));
    let panic_armed_for_transform = Arc::clone(&panic_armed);
    let panicking_transform = move |loaded: LoadedEvent| -> Option<EventEnvelope<Evt>> {
        if panic_armed_for_transform.swap(false, Ordering::SeqCst) {
            panic!("intentional poller panic");
        }
        if loaded.event_type != Evt::EVENT_TYPE {
            return None;
        }
        Some(EventEnvelope {
            aggregate_id: AggregateId::new(loaded.stream_key),
            sequence: loaded.sequence,
            global_sequence: loaded.global_sequence,
            event: Evt,
        })
    };

    let recorder = Recorder::new();
    let sub_proxy = system.spawn(Props::new(recorder.producer())).await;

    let ds = DurableStream::<EventEnvelope<Evt>>::new(
        ProcessName::new("ds-panic-topic"),
        Arc::clone(&store) as Arc<dyn EventStore>,
        panicking_transform,
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

    wait_for_count(&recorder, 3).await;
    assert_eq!(
        recorder.seen_sequences(),
        vec![1, 2, 3],
        "subscriber must receive every event after a poller panic — \
         supervision must restart the poller and the retry must replay from the \
         initial cursor"
    );

    assert!(
        !panic_armed.load(Ordering::SeqCst),
        "the one-shot panic must have been triggered to exercise the supervision path"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn durable_stream_subscribe_from_poller_stops_when_subscriber_terminates() {
    let system = ProcessSystem::new().await;
    let store = Arc::new(InMemoryEventStore::default());
    let agg_id = AggregateId::new("ds-direct-stop-agg");

    let transform_calls = Arc::new(AtomicUsize::new(0));
    let transform_calls_inner = Arc::clone(&transform_calls);
    let counting_transform = move |loaded: LoadedEvent| -> Option<EventEnvelope<Evt>> {
        transform_calls_inner.fetch_add(1, Ordering::SeqCst);
        if loaded.event_type != Evt::EVENT_TYPE {
            return None;
        }
        Some(EventEnvelope {
            aggregate_id: AggregateId::new(loaded.stream_key),
            sequence: loaded.sequence,
            global_sequence: loaded.global_sequence,
            event: Evt,
        })
    };

    let ds = DurableStream::<EventEnvelope<Evt>>::new(
        ProcessName::new("ds-direct-stop-topic"),
        Arc::clone(&store) as Arc<dyn EventStore>,
        counting_transform,
    )
    .cursor(SequenceCursor::Stream {
        key: agg_id.as_str().to_owned(),
        after: 0,
    })
    .with_poll_interval(TEST_POLL_INTERVAL)
    .spawn(&system)
    .await
    .expect("DurableStream::spawn must succeed");

    let recorder = Recorder::new();
    let sub = system.spawn(Props::new(recorder.producer())).await;
    let sub_handle = sub.clone();

    ds.subscribe_from(
        &system,
        sub,
        SequenceCursor::Stream {
            key: agg_id.as_str().to_owned(),
            after: 0,
        },
    )
    .await
    .expect("subscribe_from must succeed");

    append_evt(&store, agg_id.as_str(), 1).await;
    append_evt(&store, agg_id.as_str(), 2).await;
    wait_for_count(&recorder, 2).await;

    sub_handle
        .stop()
        .await
        .expect("subscriber stop must succeed");

    tokio::time::sleep(TEST_POLL_INTERVAL * 10).await;
    let baseline_calls = transform_calls.load(Ordering::SeqCst);

    append_evt(&store, agg_id.as_str(), 3).await;
    append_evt(&store, agg_id.as_str(), 4).await;
    tokio::time::sleep(TEST_POLL_INTERVAL * 10).await;

    assert_eq!(
        transform_calls.load(Ordering::SeqCst),
        baseline_calls,
        "subscribe_from's dedicated poller must self-stop when its subscriber \
         dies; instead it kept calling the transform after the subscriber was \
         terminated"
    );
}

#[tokio::test]
async fn durable_stream_direct_poller_observable_via_registry_lookup_by_name() {
    let system = ProcessSystem::new().await;
    let store = Arc::new(InMemoryEventStore::default());

    let ds = DurableStream::<EventEnvelope<Evt>>::new(
        ProcessName::new("ds-direct-registry-topic"),
        Arc::clone(&store) as Arc<dyn EventStore>,
        to_envelope,
    )
    .cursor(SequenceCursor::Global { after: 0 })
    .with_poll_interval(TEST_POLL_INTERVAL)
    .spawn(&system)
    .await
    .expect("DurableStream::spawn must succeed");

    let recorder = Recorder::new();
    let sub = system.spawn(Props::new(recorder.producer())).await;
    let sub_pid = sub.pid();

    ds.subscribe_from(&system, sub, SequenceCursor::Global { after: 0 })
        .await
        .expect("subscribe_from must succeed");

    let poller_name = ProcessName::new(format!("direct-poller-{sub_pid}"));
    let found = system.lookup_by_name(&poller_name).await;

    assert!(
        found.is_some(),
        "direct DurablePoller must be registered in the runtime registry under \
         '{poller_name}'"
    );
}

/// After Issue #52 the `DirectPollerProcess` watches only its subscriber, not
/// the parent `DurableStreamProxy`'s shared poller. This frees saga and
/// projection consumers from holding a `_ds_keepalive` Arc — when the
/// subscriber stops, the direct poller stops; otherwise it keeps delivering
/// regardless of whether the original `DurableStreamProxy` handle was dropped.
///
/// This test pins down the new contract: dropping the proxy by itself does
/// NOT stop the direct poller, but stopping the subscriber does.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn durable_stream_drop_proxy_keeps_direct_poller_until_subscriber_stops() {
    let system = ProcessSystem::new().await;
    let store = Arc::new(InMemoryEventStore::default());
    let agg_id = AggregateId::new("ds-drop-direct-agg");

    let transform_calls = Arc::new(AtomicUsize::new(0));
    let transform_calls_inner = Arc::clone(&transform_calls);
    let counting_transform = move |loaded: LoadedEvent| -> Option<EventEnvelope<Evt>> {
        transform_calls_inner.fetch_add(1, Ordering::SeqCst);
        if loaded.event_type != Evt::EVENT_TYPE {
            return None;
        }
        Some(EventEnvelope {
            aggregate_id: AggregateId::new(loaded.stream_key),
            sequence: loaded.sequence,
            global_sequence: loaded.global_sequence,
            event: Evt,
        })
    };

    let ds = DurableStream::<EventEnvelope<Evt>>::new(
        ProcessName::new("ds-drop-direct-topic"),
        Arc::clone(&store) as Arc<dyn EventStore>,
        counting_transform,
    )
    .cursor(SequenceCursor::Stream {
        key: agg_id.as_str().to_owned(),
        after: 0,
    })
    .with_poll_interval(TEST_POLL_INTERVAL)
    .spawn(&system)
    .await
    .expect("DurableStream::spawn must succeed");

    let recorder = Recorder::new();
    let sub = system.spawn(Props::new(recorder.producer())).await;
    let sub_for_stop = sub.clone();

    ds.subscribe_from(
        &system,
        sub,
        SequenceCursor::Stream {
            key: agg_id.as_str().to_owned(),
            after: 0,
        },
    )
    .await
    .expect("subscribe_from must succeed");

    append_evt(&store, agg_id.as_str(), 1).await;
    wait_for_count(&recorder, 1).await;

    drop(ds);
    tokio::time::sleep(TEST_POLL_INTERVAL * 6).await;

    // After ds is dropped the direct poller must keep delivering to the
    // subscriber — saga / projection rely on this contract because they drop
    // their own ds_proxy immediately after spawn.
    append_evt(&store, agg_id.as_str(), 2).await;
    wait_for_count(&recorder, 2).await;

    // Now stop the subscriber. The direct poller watches the subscriber and
    // must stop within a couple of poll cycles.
    sub_for_stop
        .stop()
        .await
        .expect("subscriber stop must succeed");
    tokio::time::sleep(TEST_POLL_INTERVAL * 6).await;
    let baseline = transform_calls.load(Ordering::SeqCst);

    append_evt(&store, agg_id.as_str(), 3).await;
    tokio::time::sleep(TEST_POLL_INTERVAL * 6).await;

    assert_eq!(
        transform_calls.load(Ordering::SeqCst),
        baseline,
        "after the subscriber stops, the direct poller must stop too; \
         instead it kept calling the transform"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn durable_stream_subscribe_from_twice_new_poller_stays_in_registry() {
    let system = ProcessSystem::new().await;
    let store = Arc::new(InMemoryEventStore::default());

    let ds = DurableStream::<EventEnvelope<Evt>>::new(
        ProcessName::new("ds-resubscribe-registry-topic"),
        Arc::clone(&store) as Arc<dyn EventStore>,
        to_envelope,
    )
    .cursor(SequenceCursor::Global { after: 0 })
    .with_poll_interval(TEST_POLL_INTERVAL)
    .spawn(&system)
    .await
    .expect("DurableStream::spawn must succeed");

    let recorder = Recorder::new();
    let sub = system.spawn(Props::new(recorder.producer())).await;
    let sub_pid = sub.pid();
    let poller_name = ProcessName::new(format!("direct-poller-{sub_pid}"));

    ds.subscribe_from(&system, sub.clone(), SequenceCursor::Global { after: 0 })
        .await
        .expect("first subscribe_from must succeed");

    ds.subscribe_from(&system, sub, SequenceCursor::Global { after: 0 })
        .await
        .expect("second subscribe_from must succeed");

    tokio::time::sleep(TEST_POLL_INTERVAL * 8).await;

    let found = system.lookup_by_name(&poller_name).await;
    assert!(
        found.is_some(),
        "lookup_by_name must return the new direct poller after the old one \
         finishes stopping; old poller's unregister must not delete the alias \
         that the new poller registered (AI-REVIEW-005)"
    );
}

/// `subscribe_from` invoked twice for the same subscriber must stop the
/// previous direct poller — otherwise both pollers would deliver duplicate
/// events to the same subscriber. The behaviour is independent of the parent
/// `DurableStreamProxy`'s lifetime (per Issue #52 the direct poller no
/// longer watches the proxy).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn durable_stream_subscribe_from_twice_stops_old_direct_poller() {
    let system = ProcessSystem::new().await;
    let store = Arc::new(InMemoryEventStore::default());
    let agg_id = AggregateId::new("ds-resubscribe-agg");

    let ds = DurableStream::<EventEnvelope<Evt>>::new(
        ProcessName::new("ds-resubscribe-topic"),
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

    let recorder = Recorder::new();
    let sub = system.spawn(Props::new(recorder.producer())).await;
    let sub_pid = sub.pid();
    let poller_name = ProcessName::new(format!("direct-poller-{sub_pid}"));

    ds.subscribe_from(
        &system,
        sub.clone(),
        SequenceCursor::Stream {
            key: agg_id.as_str().to_owned(),
            after: 0,
        },
    )
    .await
    .expect("first subscribe_from must succeed");

    // Capture the first direct poller's AnyProxy so we can later verify it
    // is no longer the one registered under the alias.
    let first_poller = system
        .lookup_by_name(&poller_name)
        .await
        .expect("first direct poller must be registered");

    ds.subscribe_from(
        &system,
        sub,
        SequenceCursor::Stream {
            key: agg_id.as_str().to_owned(),
            after: 0,
        },
    )
    .await
    .expect("second subscribe_from must succeed");

    tokio::time::sleep(TEST_POLL_INTERVAL * 6).await;

    let second_poller = system
        .lookup_by_name(&poller_name)
        .await
        .expect("a direct poller must still be registered after the second subscribe_from");

    // The new poller must have taken over the alias. The two AnyProxy
    // instances back distinct underlying processes — comparing PIDs through
    // a typed downcast is not possible here without exposing the concrete P,
    // so the regression for this is provided by the dedicated registry test
    // `durable_stream_subscribe_from_twice_new_poller_stays_in_registry`.
    // Here we additionally exercise the explicit `existing.stop()` path:
    // delivering an event after the second subscribe_from must still reach
    // the subscriber via exactly one poller (one delivery), not duplicated.
    let _ = first_poller;
    let _ = second_poller;

    append_evt(&store, agg_id.as_str(), 1).await;
    wait_for_count(&recorder, 1).await;

    // Verify only one delivery, not two. Sleep long enough for any orphan
    // poller to also re-deliver if it were still running.
    tokio::time::sleep(TEST_POLL_INTERVAL * 6).await;
    assert_eq!(
        recorder.current_count(),
        1,
        "re-calling subscribe_from must result in exactly one direct poller \
         delivering each event; observed duplicate delivery suggests an \
         orphan poller from the first subscribe_from is still running"
    );
}
