//! Integration tests for the 7.dead-letters example.
//!
//! These tests verify:
//! - `tell` to a stopped process routes to the dead-letter stream
//! - `DeadLetter.destination` matches the stopped process PID
//! - `DeadLetter.message` downcasts back to the original message type (`Ping`)
//! - `DeadLetter.sender` is `None` for a direct tell (not sent from another process)
//! - `ask` to a stopped process returns `AskError::DeadLetter` with the correct destination PID
//! - `ask` to a stopped process also publishes to the dead-letter stream
//! - Multiple `tell`s to a stopped process produce multiple dead letters on the stream
//! - `SuppressDeadLetterLog` does not prevent stream delivery; only log output is suppressed

use std::future::Future;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use nitinol_runtime::error::AskError;
use nitinol_runtime::ident::Pid;
use nitinol_runtime::process::ProcessContext;
use nitinol_runtime::{BoxedMessage, DeadLetter, ProcessSystem, Props, Subscriber};

use dead_letters::message::{Hush, Ping, Query};
use dead_letters::observer::DeadLetterObserver;
use dead_letters::target::TargetProcess;

// ─── helpers ─────────────────────────────────────────────────────────────────

/// Polls `counter` until it reaches at least `expected`, with a 5-second timeout.
async fn wait_for_count(counter: &AtomicU32, expected: u32) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while counter.load(Ordering::SeqCst) < expected {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for count to reach {expected}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

// ─── test-local subscribers ───────────────────────────────────────────────────

/// Captures the `destination` PID from the first `DeadLetter` received on the stream.
struct DestPidCapture {
    count: Arc<AtomicU32>,
    pid: Arc<tokio::sync::Mutex<Option<Pid>>>,
}

impl Subscriber<BoxedMessage> for DestPidCapture {
    fn recv(
        &mut self,
        msg: BoxedMessage,
        _ctx: &mut ProcessContext,
    ) -> impl Future<Output = ()> + Send {
        let count = self.count.clone();
        let pid = self.pid.clone();
        async move {
            let Some(dl) = msg.downcast_ref::<DeadLetter>() else {
                return;
            };
            count.fetch_add(1, Ordering::SeqCst);
            let mut guard = pid.lock().await;
            if guard.is_none() {
                *guard = Some(dl.destination);
            }
        }
    }
}

/// Records whether the inner message of the first `DeadLetter` downcasts to `T`.
///
/// Replaces the former `PingTypeCheck` and `HushTypeCheck` types which were structurally
/// identical apart from the downcast target type.
struct InnerMessageIs<T: 'static + Send + Sync> {
    count: Arc<AtomicU32>,
    matched: Arc<AtomicBool>,
    _marker: PhantomData<T>,
}

impl<T: 'static + Send + Sync> Subscriber<BoxedMessage> for InnerMessageIs<T> {
    fn recv(
        &mut self,
        msg: BoxedMessage,
        _ctx: &mut ProcessContext,
    ) -> impl Future<Output = ()> + Send {
        let count = self.count.clone();
        let matched = self.matched.clone();
        async move {
            let Some(dl) = msg.downcast_ref::<DeadLetter>() else {
                return;
            };
            count.fetch_add(1, Ordering::SeqCst);
            if dl.message.downcast_ref::<T>().is_some() {
                matched.store(true, Ordering::SeqCst);
            }
        }
    }
}

/// Records whether the `sender` field of the first `DeadLetter` is `None`.
struct SenderNoneCheck {
    count: Arc<AtomicU32>,
    sender_is_none: Arc<AtomicBool>,
}

impl Subscriber<BoxedMessage> for SenderNoneCheck {
    fn recv(
        &mut self,
        msg: BoxedMessage,
        _ctx: &mut ProcessContext,
    ) -> impl Future<Output = ()> + Send {
        let count = self.count.clone();
        let sender_is_none = self.sender_is_none.clone();
        async move {
            let Some(dl) = msg.downcast_ref::<DeadLetter>() else {
                return;
            };
            count.fetch_add(1, Ordering::SeqCst);
            if dl.sender.is_none() {
                sender_is_none.store(true, Ordering::SeqCst);
            }
        }
    }
}

// ─── tests ───────────────────────────────────────────────────────────────────

/// A `tell` to a stopped process is routed to the dead-letter stream.
#[tokio::test]
async fn tell_to_stopped_target_publishes_dead_letter_to_stream() {
    // Given: a DeadLetterObserver subscribed to the system dead-letter stream
    let system = ProcessSystem::new().await;
    let dl_stream = system.dead_letter_stream();

    let received = Arc::new(AtomicU32::new(0));
    let observer_proxy = system
        .spawn(Props::subscriber({
            let received = received.clone();
            move || DeadLetterObserver::new(received.clone())
        }))
        .await;
    dl_stream
        .subscribe(observer_proxy)
        .await
        .expect("subscribe should succeed");

    // And: a TargetProcess that has been stopped
    let proxy = system.spawn(Props::new(|| TargetProcess)).await;
    proxy.stop().await.expect("stop should succeed");
    tokio::time::sleep(Duration::from_millis(50)).await;

    // When: Ping is sent to the stopped process
    let _ = proxy.tell(Ping).await;

    // Then: the dead-letter stream delivers exactly one notification
    wait_for_count(&received, 1).await;
    assert_eq!(received.load(Ordering::SeqCst), 1);
}

/// The `destination` field of the delivered `DeadLetter` matches the stopped process PID.
#[tokio::test]
async fn dead_letter_destination_equals_stopped_target_pid() {
    // Given: a subscriber that captures the destination PID
    let system = ProcessSystem::new().await;
    let dl_stream = system.dead_letter_stream();

    let count = Arc::new(AtomicU32::new(0));
    let captured_pid: Arc<tokio::sync::Mutex<Option<Pid>>> =
        Arc::new(tokio::sync::Mutex::new(None));
    let observer_proxy = system
        .spawn(Props::subscriber({
            let count = count.clone();
            let captured_pid = captured_pid.clone();
            move || DestPidCapture {
                count: count.clone(),
                pid: captured_pid.clone(),
            }
        }))
        .await;
    dl_stream
        .subscribe(observer_proxy)
        .await
        .expect("subscribe should succeed");

    // And: a TargetProcess with a known PID that has been stopped
    let proxy = system.spawn(Props::new(|| TargetProcess)).await;
    let target_pid = proxy.pid();
    proxy.stop().await.expect("stop should succeed");
    tokio::time::sleep(Duration::from_millis(50)).await;

    // When: Ping is sent to the stopped process
    let _ = proxy.tell(Ping).await;

    // Then: the captured destination matches the stopped process PID
    wait_for_count(&count, 1).await;
    let guard = captured_pid.lock().await;
    let destination = guard.expect("destination should have been captured");
    assert_eq!(destination, target_pid);
}

/// The inner message of the `DeadLetter` downcasts back to the original message type `Ping`.
#[tokio::test]
async fn dead_letter_message_downcasts_back_to_ping() {
    // Given: a subscriber that checks whether the inner message is Ping
    let system = ProcessSystem::new().await;
    let dl_stream = system.dead_letter_stream();

    let count = Arc::new(AtomicU32::new(0));
    let inner_is_ping = Arc::new(AtomicBool::new(false));
    let observer_proxy = system
        .spawn(Props::subscriber({
            let count = count.clone();
            let inner_is_ping = inner_is_ping.clone();
            move || InnerMessageIs::<Ping> {
                count: count.clone(),
                matched: inner_is_ping.clone(),
                _marker: PhantomData,
            }
        }))
        .await;
    dl_stream
        .subscribe(observer_proxy)
        .await
        .expect("subscribe should succeed");

    // And: a stopped TargetProcess
    let proxy = system.spawn(Props::new(|| TargetProcess)).await;
    proxy.stop().await.expect("stop should succeed");
    tokio::time::sleep(Duration::from_millis(50)).await;

    // When: Ping is sent to the stopped process
    let _ = proxy.tell(Ping).await;

    // Then: the inner message downcasts successfully to Ping
    wait_for_count(&count, 1).await;
    assert!(
        inner_is_ping.load(Ordering::SeqCst),
        "DeadLetter.message should downcast to Ping"
    );
}

/// For a direct `tell`, `DeadLetter.sender` is `None` — the message was not sent from another process.
#[tokio::test]
async fn dead_letter_sender_is_none_for_direct_tell() {
    // Given: a subscriber that checks whether sender is None
    let system = ProcessSystem::new().await;
    let dl_stream = system.dead_letter_stream();

    let count = Arc::new(AtomicU32::new(0));
    let sender_is_none = Arc::new(AtomicBool::new(false));
    let observer_proxy = system
        .spawn(Props::subscriber({
            let count = count.clone();
            let sender_is_none = sender_is_none.clone();
            move || SenderNoneCheck {
                count: count.clone(),
                sender_is_none: sender_is_none.clone(),
            }
        }))
        .await;
    dl_stream
        .subscribe(observer_proxy)
        .await
        .expect("subscribe should succeed");

    // And: a stopped TargetProcess
    let proxy = system.spawn(Props::new(|| TargetProcess)).await;
    proxy.stop().await.expect("stop should succeed");
    tokio::time::sleep(Duration::from_millis(50)).await;

    // When: Ping is sent directly (not from another process)
    let _ = proxy.tell(Ping).await;

    // Then: the sender field is None
    wait_for_count(&count, 1).await;
    assert!(
        sender_is_none.load(Ordering::SeqCst),
        "DeadLetter.sender should be None for a direct tell"
    );
}

/// An `ask` to a stopped process returns `AskError::DeadLetter` with the stopped PID as destination.
#[tokio::test]
async fn ask_to_stopped_target_returns_ask_error_dead_letter_with_destination() {
    // Given: a stopped TargetProcess with a known PID
    let system = ProcessSystem::new().await;
    let proxy = system.spawn(Props::new(|| TargetProcess)).await;
    let target_pid = proxy.pid();
    proxy.stop().await.expect("stop should succeed");
    tokio::time::sleep(Duration::from_millis(50)).await;

    // When: Query is asked to the stopped process
    let result = proxy.ask(Query).await;

    // Then: AskError::DeadLetter is returned with the correct destination PID
    match result.expect_err("ask to stopped process should fail") {
        AskError::DeadLetter { destination } => {
            assert_eq!(destination, target_pid);
        }
        other => panic!("expected AskError::DeadLetter, got {:?}", other),
    }
}

/// An `ask` to a stopped process also publishes to the dead-letter stream,
/// in addition to returning `AskError::DeadLetter` to the caller.
#[tokio::test]
async fn ask_to_stopped_target_also_publishes_dead_letter_to_stream() {
    // Given: a DeadLetterObserver subscribed to the system dead-letter stream
    let system = ProcessSystem::new().await;
    let dl_stream = system.dead_letter_stream();

    let received = Arc::new(AtomicU32::new(0));
    let observer_proxy = system
        .spawn(Props::subscriber({
            let received = received.clone();
            move || DeadLetterObserver::new(received.clone())
        }))
        .await;
    dl_stream
        .subscribe(observer_proxy)
        .await
        .expect("subscribe should succeed");

    // And: a stopped TargetProcess
    let proxy = system.spawn(Props::new(|| TargetProcess)).await;
    proxy.stop().await.expect("stop should succeed");
    tokio::time::sleep(Duration::from_millis(50)).await;

    // When: Query is asked to the stopped process
    let _ = proxy.ask(Query).await;

    // Then: the dead-letter stream also receives the notification
    wait_for_count(&received, 1).await;
    assert_eq!(received.load(Ordering::SeqCst), 1);
}

/// Multiple `tell`s to a stopped process each produce a dead letter on the stream.
#[tokio::test]
async fn multiple_tells_to_stopped_target_produce_multiple_dead_letters() {
    // Given: a DeadLetterObserver subscribed to the system dead-letter stream
    let system = ProcessSystem::new().await;
    let dl_stream = system.dead_letter_stream();

    let received = Arc::new(AtomicU32::new(0));
    let observer_proxy = system
        .spawn(Props::subscriber({
            let received = received.clone();
            move || DeadLetterObserver::new(received.clone())
        }))
        .await;
    dl_stream
        .subscribe(observer_proxy)
        .await
        .expect("subscribe should succeed");

    // And: a stopped TargetProcess
    let proxy = system.spawn(Props::new(|| TargetProcess)).await;
    proxy.stop().await.expect("stop should succeed");
    tokio::time::sleep(Duration::from_millis(50)).await;

    // When: 3 Pings are sent to the stopped process
    let _ = proxy.tell(Ping).await;
    let _ = proxy.tell(Ping).await;
    let _ = proxy.tell(Ping).await;

    // Then: the dead-letter stream receives 3 separate notifications
    wait_for_count(&received, 3).await;
    assert_eq!(received.load(Ordering::SeqCst), 3);
}

/// A `Hush` message (implementing `SuppressDeadLetterLog`) is still delivered to the
/// dead-letter stream — only log output is suppressed, not stream notification.
#[tokio::test]
async fn suppress_dead_letter_log_does_not_prevent_stream_delivery() {
    // Given: a subscriber that checks whether the inner message is Hush
    let system = ProcessSystem::new().await;
    let dl_stream = system.dead_letter_stream();

    let count = Arc::new(AtomicU32::new(0));
    let inner_is_hush = Arc::new(AtomicBool::new(false));
    let observer_proxy = system
        .spawn(Props::subscriber({
            let count = count.clone();
            let inner_is_hush = inner_is_hush.clone();
            move || InnerMessageIs::<Hush> {
                count: count.clone(),
                matched: inner_is_hush.clone(),
                _marker: PhantomData,
            }
        }))
        .await;
    dl_stream
        .subscribe(observer_proxy)
        .await
        .expect("subscribe should succeed");

    // And: a stopped TargetProcess
    let proxy = system.spawn(Props::new(|| TargetProcess)).await;
    proxy.stop().await.expect("stop should succeed");
    tokio::time::sleep(Duration::from_millis(50)).await;

    // When: Hush (SuppressDeadLetterLog) is sent to the stopped process
    let _ = proxy.tell(Hush).await;

    // Then: the stream still delivers the notification (log suppression does not prevent delivery)
    wait_for_count(&count, 1).await;
    assert!(
        inner_is_hush.load(Ordering::SeqCst),
        "DeadLetter.message should downcast to Hush — SuppressDeadLetterLog must not prevent stream delivery"
    );
}
