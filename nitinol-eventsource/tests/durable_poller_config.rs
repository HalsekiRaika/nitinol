//! Integration tests for [`nitinol_eventsource::DurableSubscription::spawn_child`].
//!
//! These tests verify the contract that replaces the removed
//! `DurableStreamProxy::subscribe_from_child` API:
//!
//! - `DurableSubscription::spawn_child` delivers upstream events to the
//!   subscriber process.
//! - The spawned poller is registered as a **child** of the calling process
//!   (`ctx.children()` contains its Pid immediately after the call).
//! - When the subscriber process stops, the runtime cascade-stops the child
//!   poller automatically — it does not outlive its parent.

use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use tokio::sync::Notify;

use nitinol_eventsource::{DurableSubscription, Event, EventEnvelope, SequenceCursor};
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::{AggregateId, AppendingEvent, EventType, LoadedEvent};
use nitinol_runtime::ident::Pid;
use nitinol_runtime::process::{Process, ProcessContext, Props, Receive};
use nitinol_runtime::ProcessSystem;

#[derive(Clone)]
struct Evt;

impl Event for Evt {
    const EVENT_TYPE: EventType = EventType::from_str("DurableSubscriptionEvt");
}

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

async fn append_evt(store: &InMemoryEventStore, key: &str, sequence: u64) {
    store
        .append(
            key,
            vec![AppendingEvent {
                sequence,
                event_type: Evt::EVENT_TYPE,
                payload: Bytes::new(),
                occurred_at: jiff::Timestamp::now(),
            }],
        )
        .await
        .expect("append must succeed");
}

const TEST_POLL_INTERVAL: Duration = Duration::from_millis(50);

// ---------------------------------------------------------------------------
// Subscriber process that calls `DurableSubscription::spawn_child` in on_start
// ---------------------------------------------------------------------------

struct SubscriberViaConfig {
    config: DurableSubscription<EventEnvelope<Evt>>,
    cursor: SequenceCursor,
    count: Arc<AtomicUsize>,
    notify: Arc<Notify>,
    /// Pid of the first child spawned in on_start, recorded for the test.
    child_pid_out: Arc<tokio::sync::Mutex<Option<Pid>>>,
}

impl Process for SubscriberViaConfig {
    fn on_start(&mut self, ctx: &mut ProcessContext<Self>) -> impl Future<Output = ()> + Send {
        let config = self.config.clone();
        let cursor = self.cursor.clone();
        let child_pid_out = self.child_pid_out.clone();
        async move {
            config.spawn_child(ctx, cursor).await;
            // Record the child Pid right after spawn.
            let pid = ctx.children().iter().next().copied();
            *child_pid_out.lock().await = pid;
        }
    }
}

impl Receive<EventEnvelope<Evt>> for SubscriberViaConfig {
    type Response = ();
    type Error = std::convert::Infallible;

    async fn recv(
        &mut self,
        _msg: EventEnvelope<Evt>,
        _ctx: &mut ProcessContext<Self>,
    ) -> Result<(), Self::Error> {
        self.count.fetch_add(1, Ordering::SeqCst);
        self.notify.notify_one();
        Ok(())
    }
}

async fn wait_for_count(count: &Arc<AtomicUsize>, notify: &Arc<Notify>, expected: usize) {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let notified = notify.notified();
            if count.load(Ordering::SeqCst) >= expected {
                return;
            }
            notified.await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "timed out waiting for {expected} envelopes (received {})",
            count.load(Ordering::SeqCst)
        )
    });
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// `DurableSubscription::spawn_child` delivers upstream catchup events to the
/// subscriber process.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn durable_poller_config_delivers_catchup_events_to_subscriber() {
    let system = ProcessSystem::new().await;
    let store = Arc::new(InMemoryEventStore::default());
    let agg = AggregateId::new("dpc-catchup-agg");

    append_evt(&store, agg.as_str(), 1).await;
    append_evt(&store, agg.as_str(), 2).await;
    append_evt(&store, agg.as_str(), 3).await;

    let config = DurableSubscription::<EventEnvelope<Evt>>::new(
        Arc::clone(&store) as Arc<dyn EventStore>,
        to_envelope,
    )
    .with_poll_interval(TEST_POLL_INTERVAL);

    let count = Arc::new(AtomicUsize::new(0));
    let notify = Arc::new(Notify::new());
    let child_pid_out = Arc::new(tokio::sync::Mutex::new(None));

    let _sub = system
        .spawn({
            let c = count.clone();
            let n = notify.clone();
            let cp = child_pid_out.clone();
            let cursor = SequenceCursor::Stream {
                key: agg.as_str().to_owned(),
                after: 0,
            };
            Props::new(move || SubscriberViaConfig {
                config: config.clone(),
                cursor: cursor.clone(),
                count: c.clone(),
                notify: n.clone(),
                child_pid_out: cp.clone(),
            })
        })
        .await;

    // All three events must be delivered via the child-poller path.
    wait_for_count(&count, &notify, 3).await;
}

/// `DurableSubscription::spawn_child` registers the poller as a child of the
/// calling process — `ctx.children()` contains the poller's Pid after the call.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn durable_poller_config_registers_poller_as_child() {
    let system = ProcessSystem::new().await;
    let store = Arc::new(InMemoryEventStore::default());
    let agg = AggregateId::new("dpc-child-agg");

    let config = DurableSubscription::<EventEnvelope<Evt>>::new(
        Arc::clone(&store) as Arc<dyn EventStore>,
        to_envelope,
    )
    .with_poll_interval(TEST_POLL_INTERVAL);

    let count = Arc::new(AtomicUsize::new(0));
    let notify = Arc::new(Notify::new());
    let child_pid_out = Arc::new(tokio::sync::Mutex::new(None::<Pid>));

    let _sub = system
        .spawn({
            let c = count.clone();
            let n = notify.clone();
            let cp = child_pid_out.clone();
            let cursor = SequenceCursor::Stream {
                key: agg.as_str().to_owned(),
                after: 0,
            };
            Props::new(move || SubscriberViaConfig {
                config: config.clone(),
                cursor: cursor.clone(),
                count: c.clone(),
                notify: n.clone(),
                child_pid_out: cp.clone(),
            })
        })
        .await;

    // Wait for on_start to complete and the child Pid to be recorded.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if child_pid_out.lock().await.is_some() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for on_start to record child pid"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    let child_pid = child_pid_out
        .lock()
        .await
        .expect("child pid must be Some after on_start");

    // The child poller must be registered in the flat process registry.
    assert!(
        system.lookup(child_pid).await.is_some(),
        "poller spawned via DurableSubscription::spawn_child must appear in \
         the flat ProcessRegistry"
    );
}

/// When the subscriber stops, the runtime cascade-stops the child poller
/// automatically — the poller must not outlive its parent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn durable_poller_config_poller_stops_with_subscriber() {
    let system = ProcessSystem::new().await;
    let store = Arc::new(InMemoryEventStore::default());
    let agg = AggregateId::new("dpc-stop-agg");

    let config = DurableSubscription::<EventEnvelope<Evt>>::new(
        Arc::clone(&store) as Arc<dyn EventStore>,
        to_envelope,
    )
    .with_poll_interval(TEST_POLL_INTERVAL);

    let count = Arc::new(AtomicUsize::new(0));
    let notify = Arc::new(Notify::new());
    let child_pid_out = Arc::new(tokio::sync::Mutex::new(None::<Pid>));

    let sub = system
        .spawn({
            let c = count.clone();
            let n = notify.clone();
            let cp = child_pid_out.clone();
            let cursor = SequenceCursor::Stream {
                key: agg.as_str().to_owned(),
                after: 0,
            };
            Props::new(move || SubscriberViaConfig {
                config: config.clone(),
                cursor: cursor.clone(),
                count: c.clone(),
                notify: n.clone(),
                child_pid_out: cp.clone(),
            })
        })
        .await;

    // Wait for on_start and the child Pid.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if child_pid_out.lock().await.is_some() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for on_start to record child pid"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let child_pid = child_pid_out.lock().await.unwrap();

    // Deliver one event to confirm the poller is live.
    append_evt(&store, agg.as_str(), 1).await;
    wait_for_count(&count, &notify, 1).await;

    // Stop the subscriber — the runtime cascade-stops the child poller.
    sub.stop().await.expect("subscriber stop");

    // The child poller must unregister within a few poll cycles.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if system.lookup(child_pid).await.is_none() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "child poller must unregister after subscriber stops"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Regression for ARCH-REVIEW-006: when the subscriber process restarts, the
/// poller is re-spawned as a child on each `on_start`. If the poller called
/// `ctx.watch(subscriber)` unconditionally, each restart cycle would add a
/// *new* (stopped) poller PID to the subscriber's watcher set, causing an
/// ever-growing list of stale watchers.
///
/// The fix: the poller skips `ctx.watch` when it is already a child of the
/// subscriber (i.e. `ctx.parent() == Some(&subscriber.pid())`).  This test
/// verifies that running two restart cycles does NOT cause `on_terminated` to
/// be spuriously invoked on the subscriber after the second poller child stops.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn poller_child_restart_does_not_accumulate_stale_watchers() {
    use std::sync::atomic::AtomicBool;
    use tokio::sync::Mutex as TokioMutex;

    use nitinol_runtime::error::HandlerError;
    use nitinol_runtime::{IdleTimeout, SupervisionStrategy, Terminated};

    let system = ProcessSystem::new().await;
    let store = Arc::new(InMemoryEventStore::default());

    let on_terminated_called = Arc::new(AtomicBool::new(false));
    let on_start_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let child_pids_after_start: Arc<TokioMutex<Vec<Pid>>> = Arc::new(TokioMutex::new(Vec::new()));

    /// Request that the subscriber trigger a handler error to provoke a restart.
    struct TriggerRestart;

    struct RestartableSubscriber {
        config: DurableSubscription<EventEnvelope<Evt>>,
        on_terminated_called: Arc<AtomicBool>,
        on_start_count: Arc<std::sync::atomic::AtomicUsize>,
        child_pids: Arc<TokioMutex<Vec<Pid>>>,
    }

    impl Process for RestartableSubscriber {
        fn on_start(
            &mut self,
            ctx: &mut ProcessContext<Self>,
        ) -> impl std::future::Future<Output = ()> + Send {
            let config = self.config.clone();
            let child_pids = self.child_pids.clone();
            let count = self.on_start_count.clone();
            async move {
                let cursor = SequenceCursor::Global { after: 0 };
                config.spawn_child(ctx, cursor).await;
                let pid = ctx.children().iter().next().copied();
                child_pids.lock().await.push(pid.expect("child spawned"));
                count.fetch_add(1, Ordering::SeqCst);
            }
        }

        fn on_terminated(
            &mut self,
            _terminated: Terminated,
            _ctx: &mut ProcessContext<Self>,
        ) -> impl std::future::Future<Output = ()> + Send {
            // Must NOT be called for the child poller (it was spawned via
            // spawn_child without an explicit ctx.watch).
            let flag = self.on_terminated_called.clone();
            async move {
                flag.store(true, Ordering::SeqCst);
            }
        }
    }

    impl Receive<EventEnvelope<Evt>> for RestartableSubscriber {
        type Response = ();
        type Error = std::convert::Infallible;
        async fn recv(
            &mut self,
            _msg: EventEnvelope<Evt>,
            _ctx: &mut ProcessContext<Self>,
        ) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    impl Receive<TriggerRestart> for RestartableSubscriber {
        type Response = ();
        type Error = HandlerError;
        async fn recv(
            &mut self,
            _msg: TriggerRestart,
            _ctx: &mut ProcessContext<Self>,
        ) -> Result<(), HandlerError> {
            Err(HandlerError)
        }
    }

    let config = DurableSubscription::<EventEnvelope<Evt>>::new(
        Arc::clone(&store) as Arc<dyn nitinol_persistence::store::EventStore>,
        to_envelope,
    )
    .with_poll_interval(TEST_POLL_INTERVAL);

    let otc = on_terminated_called.clone();
    let osc = on_start_count.clone();
    let cpas = child_pids_after_start.clone();
    let mut props = Props::new(move || RestartableSubscriber {
        config: config.clone(),
        on_terminated_called: otc.clone(),
        on_start_count: osc.clone(),
        child_pids: cpas.clone(),
    });
    props.with_idle_timeout(IdleTimeout::Persistent);
    props.with_supervision_strategy(SupervisionStrategy::Restart {
        max_retries: 3,
        within: std::time::Duration::from_secs(10),
    });

    let proxy = system.spawn(props).await;

    // Wait for the first on_start to complete.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if on_start_count.load(Ordering::SeqCst) >= 1 {
            break;
        }
        assert!(Instant::now() < deadline, "timed out waiting for first on_start");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    // Trigger a restart — the old poller child is stopped, a new one spawned.
    proxy
        .tell(TriggerRestart)
        .await
        .expect("TriggerRestart tell must succeed");

    // Wait for the second on_start (restart complete).
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if on_start_count.load(Ordering::SeqCst) >= 2 {
            break;
        }
        assert!(Instant::now() < deadline, "timed out waiting for restart on_start");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    // Trigger a second restart.
    proxy
        .tell(TriggerRestart)
        .await
        .expect("second TriggerRestart tell must succeed");

    // Wait for the third on_start (second restart complete).
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if on_start_count.load(Ordering::SeqCst) >= 3 {
            break;
        }
        assert!(Instant::now() < deadline, "timed out waiting for second restart on_start");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    // Give a moment for any lingering Terminated signals to arrive.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Regression assertion: on_terminated must NOT have been called for the
    // poller child across either restart. Without the fix, the subscriber's
    // watcher set accumulates stale poller PIDs and each restart sends a
    // spurious Terminated → on_terminated call.
    assert!(
        !on_terminated_called.load(Ordering::SeqCst),
        "ARCH-REVIEW-006 regression: on_terminated must NOT be called for \
         child pollers across subscriber restarts; each poller is a hierarchy \
         child and must not register an explicit DeathWatch on the subscriber"
    );

    // Cleanup.
    proxy.stop().await.expect("final stop");
}
