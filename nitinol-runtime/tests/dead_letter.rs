mod common;

use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use nitinol_runtime::ident::{Pid, ProcessName};
use nitinol_runtime::process::{Process, ProcessContext, Receive};
use nitinol_runtime::{
    BoxError, Boxed, DeadLetter, DeadLetterResponse, Props, ProcessSystem, Stream,
    Subscriber, SuppressDeadLetterLog, subscriber_props,
};

use common::{test_props, tracked_state, wait_for_flag, GetCount, Increment};

// -- Test helpers -----------------------------------------------------------

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

/// Counts every `Boxed` message received on the dead-letter stream.
struct DeadLetterCountSubscriber {
    count: Arc<AtomicU32>,
}

impl Subscriber<Boxed> for DeadLetterCountSubscriber {
    fn recv(&mut self, _msg: Boxed, _ctx: &mut ProcessContext) -> impl Future<Output = ()> + Send {
        let count = self.count.clone();
        async move {
            count.fetch_add(1, Ordering::SeqCst);
        }
    }
}

/// Captures the `destination` PID from the first `DeadLetter` received.
struct DeadLetterDestCapture {
    captured: Arc<tokio::sync::Mutex<Option<Pid>>>,
}

impl Subscriber<Boxed> for DeadLetterDestCapture {
    fn recv(&mut self, msg: Boxed, _ctx: &mut ProcessContext) -> impl Future<Output = ()> + Send {
        let captured = self.captured.clone();
        async move {
            if let Some(dl) = msg.downcast_ref::<DeadLetter>() {
                let mut guard = captured.lock().await;
                if guard.is_none() {
                    *guard = Some(dl.destination);
                }
            }
        }
    }
}

/// Captures the raw `Boxed` from the first notification.
struct DeadLetterRawCapture {
    captured: Arc<tokio::sync::Mutex<Option<Boxed>>>,
}

impl Subscriber<Boxed> for DeadLetterRawCapture {
    fn recv(&mut self, msg: Boxed, _ctx: &mut ProcessContext) -> impl Future<Output = ()> + Send {
        let captured = self.captured.clone();
        async move {
            let mut guard = captured.lock().await;
            if guard.is_none() {
                *guard = Some(msg);
            }
        }
    }
}

/// A process + message that opts out of dead-letter log output.
struct SilentMessage;

impl SuppressDeadLetterLog for SilentMessage {}

struct SilentProcess {
    started: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
}

impl Process for SilentProcess {
    fn on_start(&mut self, _ctx: &mut ProcessContext) -> impl Future<Output = ()> + Send {
        self.started.store(true, Ordering::SeqCst);
        async {}
    }

    fn on_stop(&mut self, _ctx: &mut ProcessContext) -> impl Future<Output = ()> + Send {
        self.stopped.store(true, Ordering::SeqCst);
        async {}
    }
}

impl Receive<SilentMessage> for SilentProcess {
    type Response = ();
    async fn recv(
        &mut self,
        _msg: SilentMessage,
        _ctx: &mut ProcessContext,
    ) -> Result<(), BoxError> {
        Ok(())
    }
}

// -- System initialization tests --------------------------------------------

/// ProcessSystem::new() becomes async after Phase 1-3.
/// This test proves the API compiles and the system is usable.
#[tokio::test]
async fn process_system_new_is_async() {
    // Given / When: ProcessSystem::new() is awaited
    let system = ProcessSystem::new().await;

    // Then: system is created without panic
    drop(system);
}

/// The dead-letter stream proxy is available immediately after system startup.
#[tokio::test]
async fn dead_letter_stream_proxy_is_available_at_startup() {
    // Given: a freshly created ProcessSystem
    let system = ProcessSystem::new().await;

    // When: dead_letter_stream() is accessed
    let stream = system.dead_letter_stream();

    // Then: the proxy has a valid PID (not a panic, not a placeholder)
    let _ = stream.pid();
}

/// The dead-letter stream is registered in the process registry
/// under the well-known topic name "$dead-letters".
#[tokio::test]
async fn dead_letter_stream_is_registered_under_dollar_dead_letters() {
    // Given: a freshly created ProcessSystem
    let system = ProcessSystem::new().await;
    let topic = ProcessName::new("$dead-letters");

    // When: lookup by the well-known topic name
    let found = system.lookup_by_name(&topic).await;

    // Then: the stream process is registered
    assert!(found.is_some());
}

/// The process found under "$dead-letters" is a Stream<Boxed>.
#[tokio::test]
async fn dead_letter_stream_downcasts_to_stream_boxed() {
    // Given: the stream found by its topic name
    let system = ProcessSystem::new().await;
    let topic = ProcessName::new("$dead-letters");
    let any = system
        .lookup_by_name(&topic)
        .await
        .expect("$dead-letters should be registered");

    // When: downcast to ProcessProxy<Stream<Boxed>>
    let result = any.downcast::<Stream<Boxed>>();

    // Then: downcast succeeds — it is indeed a Stream<Boxed>
    assert!(result.is_some());
}

// -- tell → dead-letter stream routing tests --------------------------------

/// A tell to a stopped process is routed to the dead-letter stream.
/// This crosses proxy.rs → DeadLetterActor → Stream — 3 modules.
#[tokio::test]
async fn tell_to_stopped_process_routes_to_dead_letter_stream() {
    // Given: a subscriber registered to the dead-letter stream
    let system = ProcessSystem::new().await;
    let dl_stream = system.dead_letter_stream();

    let count = Arc::new(AtomicU32::new(0));
    let sub_props = subscriber_props({
        let count = count.clone();
        move || DeadLetterCountSubscriber { count: count.clone() }
    });
    let sub_proxy = system.spawn(sub_props).await;
    dl_stream
        .subscribe(sub_proxy)
        .await
        .expect("subscribe should succeed");

    // And: a process that has been stopped
    let (started, stopped, counter) = tracked_state();
    let p_props = test_props(started.clone(), stopped.clone(), counter);
    let proxy = system.spawn(p_props).await;
    wait_for_flag(&started).await;
    proxy.stop().await.expect("stop should succeed");
    wait_for_flag(&stopped).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // When: a tell is sent to the stopped process
    let _ = proxy.tell(Increment).await;

    // Then: the dead-letter subscriber receives the notification
    wait_for_count(&count, 1).await;
    assert_eq!(count.load(Ordering::SeqCst), 1);
}

/// The `DeadLetter` published to the stream contains the correct destination PID.
#[tokio::test]
async fn dead_letter_contains_correct_destination_pid() {
    // Given: a subscriber that captures the destination PID
    let system = ProcessSystem::new().await;
    let dl_stream = system.dead_letter_stream();

    let captured: Arc<tokio::sync::Mutex<Option<Pid>>> =
        Arc::new(tokio::sync::Mutex::new(None));
    let sub_props = subscriber_props({
        let captured = captured.clone();
        move || DeadLetterDestCapture { captured: captured.clone() }
    });
    let sub_proxy = system.spawn(sub_props).await;
    dl_stream
        .subscribe(sub_proxy)
        .await
        .expect("subscribe should succeed");

    // And: a stopped process with a known PID
    let (started, stopped, counter) = tracked_state();
    let p_props = test_props(started.clone(), stopped.clone(), counter);
    let proxy = system.spawn(p_props).await;
    let expected_pid = proxy.pid();
    wait_for_flag(&started).await;
    proxy.stop().await.expect("stop should succeed");
    wait_for_flag(&stopped).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // When: a tell is sent to the stopped process
    let _ = proxy.tell(Increment).await;

    // Then: the captured DeadLetter.destination matches the stopped process's PID
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        tokio::time::sleep(Duration::from_millis(5)).await;
        let guard = captured.lock().await;
        if let Some(pid) = *guard {
            assert_eq!(pid, expected_pid);
            break;
        }
        assert!(Instant::now() < deadline, "timed out waiting for dead letter");
    }
}

/// The `Boxed` published to the dead-letter stream wraps a `DeadLetter` value.
#[tokio::test]
async fn dead_letter_stream_publishes_dead_letter_type() {
    // Given: a subscriber that captures the raw Boxed
    let system = ProcessSystem::new().await;
    let dl_stream = system.dead_letter_stream();

    let captured: Arc<tokio::sync::Mutex<Option<Boxed>>> =
        Arc::new(tokio::sync::Mutex::new(None));
    let sub_props = subscriber_props({
        let captured = captured.clone();
        move || DeadLetterRawCapture { captured: captured.clone() }
    });
    let sub_proxy = system.spawn(sub_props).await;
    dl_stream
        .subscribe(sub_proxy)
        .await
        .expect("subscribe should succeed");

    // And: a stopped process
    let (started, stopped, counter) = tracked_state();
    let p_props = test_props(started.clone(), stopped.clone(), counter);
    let proxy = system.spawn(p_props).await;
    wait_for_flag(&started).await;
    proxy.stop().await.expect("stop should succeed");
    wait_for_flag(&stopped).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // When: a tell is sent to the stopped process
    let _ = proxy.tell(Increment).await;

    // Then: the captured Boxed downcasts to DeadLetter
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        tokio::time::sleep(Duration::from_millis(5)).await;
        let guard = captured.lock().await;
        if let Some(ref boxed) = *guard {
            let dl = boxed.downcast_ref::<DeadLetter>();
            assert!(dl.is_some(), "Boxed should downcast to DeadLetter");
            break;
        }
        assert!(Instant::now() < deadline, "timed out waiting for dead letter");
    }
}

/// Multiple tells to a stopped process all route to the dead-letter stream.
#[tokio::test]
async fn multiple_tells_to_stopped_process_all_route_to_dead_letter() {
    // Given: a subscriber registered to the dead-letter stream
    let system = ProcessSystem::new().await;
    let dl_stream = system.dead_letter_stream();

    let count = Arc::new(AtomicU32::new(0));
    let sub_props = subscriber_props({
        let count = count.clone();
        move || DeadLetterCountSubscriber { count: count.clone() }
    });
    let sub_proxy = system.spawn(sub_props).await;
    dl_stream
        .subscribe(sub_proxy)
        .await
        .expect("subscribe should succeed");

    // And: a stopped process
    let (started, stopped, counter) = tracked_state();
    let p_props = test_props(started.clone(), stopped.clone(), counter);
    let proxy = system.spawn(p_props).await;
    wait_for_flag(&started).await;
    proxy.stop().await.expect("stop should succeed");
    wait_for_flag(&stopped).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // When: two tell messages are sent to the stopped process
    let _ = proxy.tell(Increment).await;
    let _ = proxy.tell(Increment).await;

    // Then: both are delivered to the dead-letter stream
    wait_for_count(&count, 2).await;
    assert_eq!(count.load(Ordering::SeqCst), 2);
}

/// An ask to a stopped process also routes the message to the dead-letter stream
/// (in addition to returning DeadLetterResponse to the caller).
#[tokio::test]
async fn ask_to_stopped_process_also_routes_to_dead_letter_stream() {
    // Given: a subscriber registered to the dead-letter stream
    let system = ProcessSystem::new().await;
    let dl_stream = system.dead_letter_stream();

    let count = Arc::new(AtomicU32::new(0));
    let sub_props = subscriber_props({
        let count = count.clone();
        move || DeadLetterCountSubscriber { count: count.clone() }
    });
    let sub_proxy = system.spawn(sub_props).await;
    dl_stream
        .subscribe(sub_proxy)
        .await
        .expect("subscribe should succeed");

    // And: a process that has been stopped
    let (started, stopped, counter) = tracked_state();
    let p_props = test_props(started.clone(), stopped.clone(), counter);
    let proxy = system.spawn(p_props).await;
    wait_for_flag(&started).await;
    proxy.stop().await.expect("stop should succeed");
    wait_for_flag(&stopped).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // When: an ask is sent to the stopped process
    let _ = proxy.ask(GetCount).await;

    // Then: the dead-letter subscriber receives the stream notification
    wait_for_count(&count, 1).await;
    assert_eq!(count.load(Ordering::SeqCst), 1);
}

/// Both tell and ask to a stopped process route through the same dead-letter
/// pathway — messages arrive on the same stream with consistent count.
#[tokio::test]
async fn tell_and_ask_both_route_through_unified_dead_letter_path() {
    // Given: a subscriber counting dead-letter stream messages
    let system = ProcessSystem::new().await;
    let dl_stream = system.dead_letter_stream();

    let count = Arc::new(AtomicU32::new(0));
    let sub_props = subscriber_props({
        let count = count.clone();
        move || DeadLetterCountSubscriber { count: count.clone() }
    });
    let sub_proxy = system.spawn(sub_props).await;
    dl_stream
        .subscribe(sub_proxy)
        .await
        .expect("subscribe should succeed");

    // And: a stopped process
    let (started, stopped, counter) = tracked_state();
    let p_props = test_props(started.clone(), stopped.clone(), counter);
    let proxy = system.spawn(p_props).await;
    wait_for_flag(&started).await;
    proxy.stop().await.expect("stop should succeed");
    wait_for_flag(&stopped).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // When: both tell and ask are sent to the stopped process
    let _ = proxy.tell(Increment).await;
    let _ = proxy.ask(GetCount).await;

    // Then: both messages are delivered to the dead-letter stream
    wait_for_count(&count, 2).await;
    assert_eq!(count.load(Ordering::SeqCst), 2);
}

// -- ask → DeadLetterResponse tests ----------------------------------------

/// An ask to a stopped process returns a DeadLetterResponse error.
#[tokio::test]
async fn ask_to_stopped_process_returns_dead_letter_response() {
    // Given: a process that has been stopped
    let system = ProcessSystem::new().await;
    let (started, stopped, counter) = tracked_state();
    let props = test_props(started.clone(), stopped.clone(), counter);
    let proxy = system.spawn(props).await;
    wait_for_flag(&started).await;
    proxy.stop().await.expect("stop should succeed");
    wait_for_flag(&stopped).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // When: an ask is sent to the stopped process
    let result = proxy.ask(GetCount).await;

    // Then: the error downcasts to DeadLetterResponse (not a generic "process already stopped")
    assert!(result.is_err());
    let err = result.unwrap_err();
    let dl_response = err.downcast::<DeadLetterResponse>();
    assert!(
        dl_response.is_ok(),
        "error should be DeadLetterResponse, got a different type"
    );
}

/// The DeadLetterResponse returned by ask contains the destination PID.
#[tokio::test]
async fn dead_letter_response_contains_destination_pid() {
    // Given: a stopped process with a known PID
    let system = ProcessSystem::new().await;
    let (started, stopped, counter) = tracked_state();
    let props = test_props(started.clone(), stopped.clone(), counter);
    let proxy = system.spawn(props).await;
    let expected_pid = proxy.pid();
    wait_for_flag(&started).await;
    proxy.stop().await.expect("stop should succeed");
    wait_for_flag(&stopped).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // When: ask is sent and the error is extracted
    let err = proxy.ask(GetCount).await.unwrap_err();
    let dl_response = err
        .downcast::<DeadLetterResponse>()
        .expect("should be DeadLetterResponse");

    // Then: the destination PID matches the stopped process
    assert_eq!(dl_response.destination, expected_pid);
}

// -- SuppressDeadLetterLog tests --------------------------------------------

/// A message type implementing SuppressDeadLetterLog is still published to
/// the dead-letter stream — only log output is suppressed, not notification.
#[tokio::test]
async fn suppressed_message_is_still_delivered_to_dead_letter_stream() {
    // Given: a subscriber registered to the dead-letter stream
    let system = ProcessSystem::new().await;
    let dl_stream = system.dead_letter_stream();

    let count = Arc::new(AtomicU32::new(0));
    let sub_props = subscriber_props({
        let count = count.clone();
        move || DeadLetterCountSubscriber { count: count.clone() }
    });
    let sub_proxy = system.spawn(sub_props).await;
    dl_stream
        .subscribe(sub_proxy)
        .await
        .expect("subscribe should succeed");

    // And: a SilentProcess (handles SilentMessage: SuppressDeadLetterLog) that is stopped
    let started = Arc::new(AtomicBool::new(false));
    let stopped = Arc::new(AtomicBool::new(false));
    let proxy = system
        .spawn(Props::new({
            let started = started.clone();
            let stopped = stopped.clone();
            move || SilentProcess {
                started: started.clone(),
                stopped: stopped.clone(),
            }
        }))
        .await;
    wait_for_flag(&started).await;
    proxy.stop().await.expect("stop should succeed");
    wait_for_flag(&stopped).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // When: a SilentMessage (SuppressDeadLetterLog) is sent to the stopped process
    let _ = proxy.tell(SilentMessage).await;

    // Then: the dead-letter stream still receives the notification
    wait_for_count(&count, 1).await;
    assert_eq!(count.load(Ordering::SeqCst), 1);
}

// -- Log throttle tests ----------------------------------------------------

/// The log throttle suppresses log output for high-volume dead letters,
/// but stream delivery is never blocked — all messages arrive.
#[tokio::test]
async fn log_throttle_does_not_prevent_stream_delivery() {
    // Given: a subscriber registered to the dead-letter stream
    let system = ProcessSystem::new().await;
    let dl_stream = system.dead_letter_stream();

    let count = Arc::new(AtomicU32::new(0));
    let sub_props = subscriber_props({
        let count = count.clone();
        move || DeadLetterCountSubscriber { count: count.clone() }
    });
    let sub_proxy = system.spawn(sub_props).await;
    dl_stream
        .subscribe(sub_proxy)
        .await
        .expect("subscribe should succeed");

    // And: a stopped process
    let (started, stopped, counter) = tracked_state();
    let p_props = test_props(started.clone(), stopped.clone(), counter);
    let proxy = system.spawn(p_props).await;
    wait_for_flag(&started).await;
    proxy.stop().await.expect("stop should succeed");
    wait_for_flag(&stopped).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // When: many messages are sent — beyond the per-window log throttle limit (default 10/10s)
    let msg_count = 20u32;
    for _ in 0..msg_count {
        let _ = proxy.tell(Increment).await;
    }

    // Then: all messages are delivered to the stream regardless of log throttling
    wait_for_count(&count, msg_count).await;
    assert_eq!(count.load(Ordering::SeqCst), msg_count);
}
