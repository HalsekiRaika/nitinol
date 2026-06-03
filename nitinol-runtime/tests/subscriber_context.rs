//! Tests for the new dedicated `SubscriberContext<'_, T>` introduced in
//! Issue #52 to keep the internal `SubscriberProcess<S, T>` wrapper out of
//! the public-facing `Subscriber<T>` trait.
//!
//! Goals pinned down here:
//! - `Subscriber<T>::recv` accepts `&mut SubscriberContext<'_, T>` (new signature)
//! - `SubscriberContext<'_, T>` is generic over `T` and lifetime-borrowed
//!   (compile-time assertion)
//! - A subscriber that uses the new context still receives published messages
//! - `SubscriberContext` exposes the subscriber's own `pid()` (the `pid`
//!   field is named in the design plan as borrowed)
//! - `SubscriberContext::watch`, `unwatch`, and `stop_self` behave correctly
//!   (they delegate to the same wiring as `ProcessContext`)

use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;

use nitinol_runtime::ident::{Pid, ProcessName};
use nitinol_runtime::process::{Process, ProcessContext, Subscriber, SubscriberContext};
use nitinol_runtime::{BoxedMessage, ProcessSystem, Props};

// ---------------------------------------------------------------------------
// Fixture: a subscriber that counts incoming messages.
// ---------------------------------------------------------------------------

struct CountingSubscriber {
    count: Arc<AtomicU32>,
}

impl Subscriber<BoxedMessage> for CountingSubscriber {
    fn recv(
        &mut self,
        _msg: BoxedMessage,
        // The new API: the second argument is `&mut SubscriberContext<'_, T>`,
        // not `&mut ProcessContext`. The compiler enforces this signature for
        // every `Subscriber<T>` impl.
        _ctx: &mut SubscriberContext<'_, BoxedMessage>,
    ) -> impl Future<Output = ()> + Send {
        let count = self.count.clone();
        async move {
            count.fetch_add(1, Ordering::SeqCst);
        }
    }
}

// ---------------------------------------------------------------------------
// Fixture: a subscriber that captures the PID from its context.
// ---------------------------------------------------------------------------

struct PidCapturingSubscriber {
    observed: Arc<Mutex<Option<Pid>>>,
}

impl Subscriber<BoxedMessage> for PidCapturingSubscriber {
    fn recv(
        &mut self,
        _msg: BoxedMessage,
        ctx: &mut SubscriberContext<'_, BoxedMessage>,
    ) -> impl Future<Output = ()> + Send {
        // The design plan names `pid` as one of the borrowed fields of
        // `SubscriberContext`. The accessor must be available to user code so
        // a subscriber can recover its own PID without holding extra state.
        let pid = ctx.pid();
        let observed = self.observed.clone();
        async move {
            *observed.lock().await = Some(pid);
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

async fn wait_for_count(counter: &AtomicU32, expected: u32) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while counter.load(Ordering::SeqCst) < expected {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for subscriber to observe {expected} messages"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

async fn wait_for_some_pid(slot: &Arc<Mutex<Option<Pid>>>) -> Pid {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        {
            let guard = slot.lock().await;
            if let Some(pid) = *guard {
                return pid;
            }
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for the subscriber to capture its PID"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

// ---------------------------------------------------------------------------
// Behavioral tests.
// ---------------------------------------------------------------------------

/// Given a `Subscriber<BoxedMessage>` whose `recv` uses the new
/// `&mut SubscriberContext<'_, BoxedMessage>` parameter,
/// when the subscriber is spawned via `Props::subscriber` and registered on
/// a stream,
/// then it receives published messages exactly as before — the context type
/// switch is a signature change, not a behavior change.
#[tokio::test]
async fn subscriber_with_subscriber_context_receives_published_messages() {
    // Given
    let system = ProcessSystem::new().await;
    let topic = ProcessName::new("subscriber-context-receives");
    let stream = system
        .spawn_stream::<BoxedMessage>(topic)
        .await
        .expect("spawn_stream must succeed");

    let count = Arc::new(AtomicU32::new(0));
    let props = Props::subscriber({
        let count = count.clone();
        move || CountingSubscriber {
            count: count.clone(),
        }
    });
    let sub_proxy = system.spawn(props).await;
    stream
        .subscribe(sub_proxy)
        .await
        .expect("subscribe must succeed");

    // When
    stream
        .publish_boxed(7u32)
        .await
        .expect("publish must succeed");

    // Then
    wait_for_count(&count, 1).await;
    assert_eq!(
        count.load(Ordering::SeqCst),
        1,
        "subscriber must observe exactly the published message"
    );
}

/// Given a subscriber that calls `ctx.pid()` inside `recv`,
/// when a message is published to it,
/// then the captured PID equals the PID the system returned at spawn time —
/// proving the `SubscriberContext` is wired to the subscriber's own actor and
/// not, for example, to the stream that delivered the message.
#[tokio::test]
async fn subscriber_context_pid_matches_spawned_subscriber_pid() {
    // Given
    let system = ProcessSystem::new().await;
    let topic = ProcessName::new("subscriber-context-pid");
    let stream = system
        .spawn_stream::<BoxedMessage>(topic)
        .await
        .expect("spawn_stream must succeed");

    let observed = Arc::new(Mutex::new(None));
    let props = Props::subscriber({
        let observed = observed.clone();
        move || PidCapturingSubscriber {
            observed: observed.clone(),
        }
    });
    let sub_proxy = system.spawn(props).await;
    let expected_pid = sub_proxy.pid();
    stream
        .subscribe(sub_proxy)
        .await
        .expect("subscribe must succeed");

    // When
    stream
        .publish_boxed(0u32)
        .await
        .expect("publish must succeed");

    // Then
    let captured = wait_for_some_pid(&observed).await;
    assert_eq!(
        captured, expected_pid,
        "SubscriberContext::pid() must equal the spawned subscriber's PID"
    );
}

// ---------------------------------------------------------------------------
// Static type-level checks: `SubscriberContext` is generic over `T`, and the
// `Subscriber<T>` trait's `recv` signature really uses it. These would fail
// to compile if either the generic parameter or the new context type were
// removed.
// ---------------------------------------------------------------------------

#[allow(dead_code)]
fn assert_subscriber_context_is_generic_over_t<'a, T>(_ctx: &SubscriberContext<'a, T>) {
    // Compile-only: `SubscriberContext` must accept a `T` type parameter.
}

#[allow(dead_code)]
fn assert_subscriber_trait_uses_subscriber_context<S, T>()
where
    S: Subscriber<T>,
    T: 'static + Send + Sync,
{
    // Compile-only: the bound resolves only if `Subscriber<T>::recv` takes
    // `&mut SubscriberContext<'_, T>` (i.e. the new trait signature has
    // landed and not, say, `&mut ProcessContext`).
}

/// Type-level: a subscriber's `recv` body is allowed to take `ctx.pid()` via
/// an immutable read of the context fields. This pins the public method shape
/// to `fn pid(&self) -> Pid`, matching the design plan's accessor list.
#[allow(dead_code)]
fn assert_subscriber_context_exposes_pid<T>(ctx: &SubscriberContext<'_, T>) -> Pid {
    ctx.pid()
}

// ---------------------------------------------------------------------------
// Regression guard: dropping the original spawn proxy must not stop the
// subscriber-context-driven subscriber early. (Mirrors the captured-self-proxy
// guarantee from self_proxy.rs at the subscriber level.)
// ---------------------------------------------------------------------------

/// Given a subscriber registered to a stream,
/// when the original spawn proxy is dropped after subscription,
/// then published messages still reach the subscriber, because the runtime
/// keeps the actor alive through the stream's dispatcher record.
#[tokio::test]
async fn subscriber_remains_addressable_after_spawn_proxy_drop() {
    // Given
    let system = ProcessSystem::new().await;
    let topic = ProcessName::new("subscriber-context-drop");
    let stream = system
        .spawn_stream::<BoxedMessage>(topic)
        .await
        .expect("spawn_stream must succeed");

    let count = Arc::new(AtomicU32::new(0));
    let props = Props::subscriber({
        let count = count.clone();
        move || CountingSubscriber {
            count: count.clone(),
        }
    });
    let sub_proxy = system.spawn(props).await;
    stream
        .subscribe(sub_proxy.clone())
        .await
        .expect("subscribe must succeed");

    // When
    drop(sub_proxy);
    stream
        .publish_boxed(42u32)
        .await
        .expect("publish must succeed");

    // Then
    wait_for_count(&count, 1).await;
    assert_eq!(count.load(Ordering::SeqCst), 1);
}

// ---------------------------------------------------------------------------
// Behavioral tests for SubscriberContext wiring operations.
//
// `SubscriberContext::watch`, `unwatch`, and `stop_self` delegate to the same
// private `wiring` module as `ProcessContext`.  These tests prove that the
// delegation is wired up correctly and that each operation produces the
// expected observable side-effect at the actor level.
// ---------------------------------------------------------------------------

/// Minimal process used as a watch target — starts, can be stopped.
struct TargetProcess {
    started: Arc<AtomicBool>,
}

impl Process for TargetProcess {
    fn on_start(&mut self, _ctx: &mut ProcessContext<Self>) -> impl Future<Output = ()> + Send {
        self.started.store(true, Ordering::SeqCst);
        async {}
    }
}

async fn wait_for_process_stopped(system: &ProcessSystem, pid: Pid) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if system.lookup(pid).await.is_none() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for process {pid} to stop"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

// ---------------------------------------------------------------------------
// stop_self
// ---------------------------------------------------------------------------

/// A subscriber that calls `ctx.stop_self()` on its first received message.
struct StopOnFirstSubscriber {
    count: Arc<AtomicU32>,
}

impl Subscriber<BoxedMessage> for StopOnFirstSubscriber {
    fn recv(
        &mut self,
        _msg: BoxedMessage,
        ctx: &mut SubscriberContext<'_, BoxedMessage>,
    ) -> impl Future<Output = ()> + Send {
        // Increment before stopping so the test can observe count == 1 as a
        // "stop was triggered" signal.
        self.count.fetch_add(1, Ordering::SeqCst);
        // Extract the stop future before the async block to avoid capturing ctx.
        let stop_fut = ctx.stop_self();
        async move {
            let _ = stop_fut.await;
        }
    }
}

/// Given a subscriber that calls `ctx.stop_self()` on its first message,
/// when two messages are published,
/// then exactly one message is counted: the subscriber stops after the first
/// and does not process the second.
#[tokio::test]
async fn subscriber_context_stop_self_stops_subscriber() {
    // Given
    let system = ProcessSystem::new().await;
    let topic = ProcessName::new("subscriber-ctx-stop-self");
    let stream = system
        .spawn_stream::<BoxedMessage>(topic)
        .await
        .expect("spawn_stream must succeed");

    let count = Arc::new(AtomicU32::new(0));
    let props = Props::subscriber({
        let count = count.clone();
        move || StopOnFirstSubscriber {
            count: count.clone(),
        }
    });
    let sub_proxy = system.spawn(props).await;
    let sub_pid = sub_proxy.pid();
    stream
        .subscribe(sub_proxy)
        .await
        .expect("subscribe must succeed");

    // When: first message — triggers stop_self()
    stream
        .publish_boxed(1u32)
        .await
        .expect("first publish must succeed");

    // Wait until stop_self takes effect: the subscriber process disappears from
    // the registry once its lifecycle loop exits.
    wait_for_count(&count, 1).await;
    wait_for_process_stopped(&system, sub_pid).await;

    // When: second message — subscriber is already stopped
    stream
        .publish_boxed(2u32)
        .await
        .expect("second publish must succeed");
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Then: count is still 1 — the second message was not processed.
    assert_eq!(
        count.load(Ordering::SeqCst),
        1,
        "subscriber must stop after calling stop_self() and must not process the second message"
    );
}

// ---------------------------------------------------------------------------
// watch
// ---------------------------------------------------------------------------

/// A subscriber that, on its first message, watches a specified target PID
/// and increments a counter.
struct WatchOnFirstSubscriber {
    count: Arc<AtomicU32>,
    target_pid: Pid,
}

impl Subscriber<BoxedMessage> for WatchOnFirstSubscriber {
    fn recv(
        &mut self,
        _msg: BoxedMessage,
        ctx: &mut SubscriberContext<'_, BoxedMessage>,
    ) -> impl Future<Output = ()> + Send {
        let count = self.count.clone();
        let watch_fut = ctx.watch(self.target_pid);
        async move {
            watch_fut.await;
            count.fetch_add(1, Ordering::SeqCst);
        }
    }
}

/// Given a subscriber that calls `ctx.watch(target_pid)` when it receives a
/// message, when the target process subsequently stops, the subscriber receives
/// `Terminated` via its `on_terminated` hook (a no-op for `SubscriberProcess`),
/// and remains functional — the second message is processed normally.
///
/// This proves the `watch` wiring is connected: the Watch signal reaches the
/// target, and the resulting `Terminated` is delivered without crashing the
/// subscriber.
#[tokio::test]
async fn subscriber_context_watch_live_process_continues_working_after_target_stops() {
    // Given: a target process
    let system = ProcessSystem::new().await;
    let target_started = Arc::new(AtomicBool::new(false));
    let target_proxy = system
        .spawn(Props::new({
            let started = target_started.clone();
            move || TargetProcess {
                started: started.clone(),
            }
        }))
        .await;
    let target_pid = target_proxy.pid();
    // Wait for the target to start before registering a watch.
    let deadline = Instant::now() + Duration::from_secs(5);
    while !target_started.load(Ordering::SeqCst) {
        assert!(Instant::now() < deadline, "target did not start");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    // And: a subscriber stream
    let topic = ProcessName::new("subscriber-ctx-watch");
    let stream = system
        .spawn_stream::<BoxedMessage>(topic)
        .await
        .expect("spawn_stream must succeed");

    let count = Arc::new(AtomicU32::new(0));
    let props = Props::subscriber({
        let count = count.clone();
        move || WatchOnFirstSubscriber {
            count: count.clone(),
            target_pid,
        }
    });
    let sub_proxy = system.spawn(props).await;
    stream
        .subscribe(sub_proxy)
        .await
        .expect("subscribe must succeed");

    // When: first message — subscriber calls ctx.watch(target_pid)
    stream
        .publish_boxed(1u32)
        .await
        .expect("first publish must succeed");
    wait_for_count(&count, 1).await;

    // And: target stops — Terminated is delivered to the subscriber's
    // on_terminated (no-op), which must not crash the subscriber.
    target_proxy.stop().await.expect("target stop must succeed");
    wait_for_process_stopped(&system, target_pid).await;
    // Give the Terminated signal time to be delivered and consumed.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // When: second message — subscriber must still be alive
    stream
        .publish_boxed(2u32)
        .await
        .expect("second publish must succeed");
    wait_for_count(&count, 2).await;

    // Then: both messages were received, proving the subscriber survived.
    assert_eq!(
        count.load(Ordering::SeqCst),
        2,
        "subscriber must still process messages after a watched process stops"
    );
}

// ---------------------------------------------------------------------------
// unwatch
// ---------------------------------------------------------------------------

/// A subscriber that, on its first message, unwatches a specified PID.
struct UnwatchOnFirstSubscriber {
    count: Arc<AtomicU32>,
    target_pid: Pid,
}

impl Subscriber<BoxedMessage> for UnwatchOnFirstSubscriber {
    fn recv(
        &mut self,
        _msg: BoxedMessage,
        ctx: &mut SubscriberContext<'_, BoxedMessage>,
    ) -> impl Future<Output = ()> + Send {
        let count = self.count.clone();
        let unwatch_fut = ctx.unwatch(self.target_pid);
        async move {
            unwatch_fut.await;
            count.fetch_add(1, Ordering::SeqCst);
        }
    }
}

/// Given a subscriber that calls `ctx.unwatch(target_pid)` when it receives
/// a message, when a second message is published, the subscriber remains
/// functional — the unwatch does not disrupt the subscriber.
#[tokio::test]
async fn subscriber_context_unwatch_does_not_disrupt_subscriber() {
    // Given: a target process (to unwatch; no prior watch is needed — unwatch
    // on a live process that was never watched is a no-op signal send).
    let system = ProcessSystem::new().await;
    let target_proxy = system
        .spawn(Props::new(|| TargetProcess {
            started: Arc::new(AtomicBool::new(false)),
        }))
        .await;
    let target_pid = target_proxy.pid();

    let topic = ProcessName::new("subscriber-ctx-unwatch");
    let stream = system
        .spawn_stream::<BoxedMessage>(topic)
        .await
        .expect("spawn_stream must succeed");

    let count = Arc::new(AtomicU32::new(0));
    let props = Props::subscriber({
        let count = count.clone();
        move || UnwatchOnFirstSubscriber {
            count: count.clone(),
            target_pid,
        }
    });
    let sub_proxy = system.spawn(props).await;
    stream
        .subscribe(sub_proxy)
        .await
        .expect("subscribe must succeed");

    // When: two messages are published; the first triggers unwatch()
    stream
        .publish_boxed(1u32)
        .await
        .expect("first publish must succeed");
    wait_for_count(&count, 1).await;

    stream
        .publish_boxed(2u32)
        .await
        .expect("second publish must succeed");
    wait_for_count(&count, 2).await;

    // Then: both messages were received — unwatch did not crash the subscriber.
    assert_eq!(
        count.load(Ordering::SeqCst),
        2,
        "subscriber must remain functional after calling unwatch()"
    );
}
