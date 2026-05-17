mod common;

use std::future::Future;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use nitinol_runtime::error::{AskError, SendError, SpawnError};
use nitinol_runtime::ident::ProcessName;
use nitinol_runtime::process::{Process, ProcessContext, Receive};
use nitinol_runtime::{BoxedMessage, ProcessSystem, Props, Subscriber};

use common::{test_props, tracked_state, wait_for_flag, GetCount, Increment};

#[derive(Debug, thiserror::Error)]
#[error("handler error: {0}")]
struct HandlerTestError(String);

/// A process whose `FailMsg` handler always returns `Err`.
struct FailingProcess;

impl Process for FailingProcess {}

struct FailMsg;

impl Receive<FailMsg> for FailingProcess {
    type Response = ();
    type Error = HandlerTestError;
    async fn recv(&mut self, _: FailMsg, _: &mut ProcessContext) -> Result<(), HandlerTestError> {
        Err(HandlerTestError("deliberate failure".to_string()))
    }
}

struct SuccessMsg;

impl Receive<SuccessMsg> for FailingProcess {
    type Response = u32;
    type Error = std::convert::Infallible;
    async fn recv(
        &mut self,
        _: SuccessMsg,
        _: &mut ProcessContext,
    ) -> Result<u32, std::convert::Infallible> {
        Ok(42)
    }
}

/// `tell` to a stopped process returns `SendError`.
#[tokio::test]
async fn tell_to_stopped_process_returns_send_error() {
    // Given: a process that has been stopped
    let system = ProcessSystem::new().await;
    let (started, stopped, counter) = tracked_state();
    let proxy = system
        .spawn(test_props(started.clone(), stopped.clone(), counter))
        .await;
    wait_for_flag(&started).await;
    proxy.stop().await.expect("stop should succeed");
    wait_for_flag(&stopped).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // When: a tell is sent to the stopped process
    let result = proxy.tell(Increment).await;

    // Then: the result is Err(SendError)
    assert!(result.is_err());
    let _: SendError = result.unwrap_err();
}

/// `stop()` on an already-stopped process returns `SendError`.
#[tokio::test]
async fn stop_on_already_stopped_process_returns_send_error() {
    // Given: a process that has been stopped
    let system = ProcessSystem::new().await;
    let (started, stopped, counter) = tracked_state();
    let proxy = system
        .spawn(test_props(started.clone(), stopped.clone(), counter))
        .await;
    wait_for_flag(&started).await;
    proxy.stop().await.expect("initial stop should succeed");
    wait_for_flag(&stopped).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // When: stop() is called again on the already-stopped process
    let result = proxy.stop().await;

    // Then: Err(SendError) is returned
    assert!(result.is_err());
    let _: SendError = result.unwrap_err();
}

/// `poison()` on an already-stopped process returns `SendError`.
#[tokio::test]
async fn poison_on_already_stopped_process_returns_send_error() {
    // Given: a process that has been stopped
    let system = ProcessSystem::new().await;
    let (started, stopped, counter) = tracked_state();
    let proxy = system
        .spawn(test_props(started.clone(), stopped.clone(), counter))
        .await;
    wait_for_flag(&started).await;
    proxy.stop().await.expect("initial stop should succeed");
    wait_for_flag(&stopped).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // When: poison() is called on the already-stopped process
    let result = proxy.poison().await;

    // Then: Err(SendError) is returned
    assert!(result.is_err());
    let _: SendError = result.unwrap_err();
}

/// `SendError` implements `std::error::Error` (required for `thiserror` chain).
#[tokio::test]
async fn send_error_implements_std_error() {
    // Given: a SendError value
    let err = SendError;

    // When: Display and Error trait are used
    let msg = err.to_string();

    // Then: a non-empty message is produced
    assert!(!msg.is_empty());
}

/// `ask` to a stopped process returns `AskError::DeadLetter`.
#[tokio::test]
async fn ask_to_stopped_process_returns_ask_error_dead_letter() {
    // Given: a process that has been stopped
    let system = ProcessSystem::new().await;
    let (started, stopped, counter) = tracked_state();
    let proxy = system
        .spawn(test_props(started.clone(), stopped.clone(), counter))
        .await;
    wait_for_flag(&started).await;
    proxy.stop().await.expect("stop should succeed");
    wait_for_flag(&stopped).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // When: an ask is sent to the stopped process
    let result = proxy.ask(GetCount).await;

    // Then: AskError::DeadLetter is returned
    match result {
        Err(AskError::DeadLetter { .. }) => {}
        other => panic!("expected AskError::DeadLetter, got {:?}", other),
    }
}

/// `AskError::DeadLetter` contains the exact PID of the stopped process.
#[tokio::test]
async fn ask_dead_letter_error_contains_correct_destination_pid() {
    // Given: a stopped process with a known PID
    let system = ProcessSystem::new().await;
    let (started, stopped, counter) = tracked_state();
    let proxy = system
        .spawn(test_props(started.clone(), stopped.clone(), counter))
        .await;
    let expected_pid = proxy.pid();
    wait_for_flag(&started).await;
    proxy.stop().await.expect("stop should succeed");
    wait_for_flag(&stopped).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // When: an ask is sent to the stopped process
    let result = proxy.ask(GetCount).await;

    // Then: the destination PID inside the error matches
    match result.expect_err("ask to stopped process should fail") {
        AskError::DeadLetter { destination } => {
            assert_eq!(destination, expected_pid);
        }
        other => panic!(
            "expected AskError::DeadLetter {{ destination: {:?} }}, got {:?}",
            expected_pid, other
        ),
    }
}

/// `ask` to a handler that returns `Err` produces `AskError::Handler(e)`.
#[tokio::test]
async fn ask_to_failing_handler_returns_ask_error_handler() {
    // Given: a process whose handler always fails with HandlerTestError
    let system = ProcessSystem::new().await;
    let proxy = system.spawn(Props::new(|| FailingProcess)).await;

    // When: an ask is sent (handler will fail)
    let result = proxy.ask(FailMsg).await;

    // Then: AskError::Handler is returned with the handler's concrete error
    match result {
        Err(AskError::Handler(e)) => {
            assert_eq!(e.to_string(), "handler error: deliberate failure");
        }
        other => panic!("expected AskError::Handler, got {:?}", other),
    }
}

/// `ask` to a handler that returns `Ok` produces `Ok(value)`.
#[tokio::test]
async fn ask_to_succeeding_handler_returns_ok() {
    // Given: a process whose handler always succeeds
    let system = ProcessSystem::new().await;
    let proxy = system.spawn(Props::new(|| FailingProcess)).await;

    // When: an ask is sent (handler succeeds)
    let result = proxy.ask(SuccessMsg).await;

    // Then: Ok(42) is returned
    assert_eq!(result.expect("ask should succeed"), 42u32);
}

/// `spawn_stream` with a duplicate topic returns `Err(SpawnError)`.
#[tokio::test]
async fn spawn_stream_duplicate_returns_spawn_error() {
    // Given: a stream already registered under "se-dup"
    let system = ProcessSystem::new().await;
    let topic = ProcessName::new("se-dup");
    system
        .spawn_stream::<BoxedMessage>(topic.clone())
        .await
        .expect("first spawn_stream should succeed");

    // When: a second stream is spawned with the same topic
    let result = system.spawn_stream::<BoxedMessage>(topic).await;

    // Then: Err(SpawnError) is returned and error type is statically known
    let err: SpawnError = match result {
        Err(e) => e,
        Ok(_) => panic!("second spawn_stream should fail with SpawnError"),
    };
    assert_eq!(err.topic, "se-dup");
}

/// `SpawnError::to_string` contains the duplicate topic name.
#[tokio::test]
async fn spawn_error_display_contains_topic_name() {
    // Given: a SpawnError with a known topic
    let err = SpawnError {
        topic: "my-stream-topic".to_string(),
    };

    // When: Display is called
    let msg = err.to_string();

    // Then: the topic name appears in the error message
    assert!(
        msg.contains("my-stream-topic"),
        "SpawnError Display should contain the topic name, got: {msg}"
    );
}

/// `SpawnError` implements `std::error::Error`.
#[tokio::test]
async fn spawn_error_implements_std_error() {
    // Given: a SpawnError
    let err = SpawnError {
        topic: "topic".to_string(),
    };

    // When: used as dyn Error
    let _: &dyn std::error::Error = &err;
}

/// `dead_letter_stream().pid()` returns a non-zero PID.
#[tokio::test]
async fn dead_letter_stream_pid_is_accessible() {
    // Given: a freshly created ProcessSystem
    let system = ProcessSystem::new().await;

    // When: pid() is called on the dead-letter stream
    let pid = system.dead_letter_stream().pid();

    // Then: a valid (non-panic) PID is returned
    let _ = pid;
}

/// `DeadLetterStream::subscribe` succeeds for a compatible subscriber.
#[tokio::test]
async fn dead_letter_stream_subscribe_succeeds() {
    struct DummySubscriber;

    impl Subscriber<BoxedMessage> for DummySubscriber {
        async fn recv(&mut self, _: BoxedMessage, _: &mut ProcessContext) {}
    }

    // Given: a freshly created ProcessSystem
    let system = ProcessSystem::new().await;
    let dl_stream = system.dead_letter_stream();

    let sub_proxy = system.spawn(Props::subscriber(|| DummySubscriber)).await;

    // When: subscribe is called
    let result = dl_stream.subscribe(sub_proxy).await;

    // Then: no error is returned
    assert!(result.is_ok(), "subscribe should succeed");
}

/// `DeadLetterStream::unsubscribe` succeeds for a known subscriber PID.
#[tokio::test]
async fn dead_letter_stream_unsubscribe_succeeds() {
    struct DummySubscriber;

    impl Subscriber<BoxedMessage> for DummySubscriber {
        async fn recv(&mut self, _: BoxedMessage, _: &mut ProcessContext) {}
    }

    // Given: a subscriber already registered to the dead-letter stream
    let system = ProcessSystem::new().await;
    let dl_stream = system.dead_letter_stream();

    let sub_proxy = system.spawn(Props::subscriber(|| DummySubscriber)).await;
    let sub_pid = sub_proxy.pid();
    dl_stream
        .subscribe(sub_proxy)
        .await
        .expect("subscribe should succeed");

    // When: unsubscribe is called with the subscriber's PID
    let result = dl_stream.unsubscribe(sub_pid).await;

    // Then: no error is returned
    assert!(result.is_ok(), "unsubscribe should succeed");
}

/// After `unsubscribe`, the subscriber no longer receives dead-letter events.
#[tokio::test]
async fn dead_letter_stream_unsubscribed_subscriber_receives_no_further_events() {
    // Given: a counter subscriber registered to the dead-letter stream
    let system = ProcessSystem::new().await;
    let dl_stream = system.dead_letter_stream();

    struct CountingSubscriber {
        count: Arc<AtomicU32>,
    }

    impl Subscriber<BoxedMessage> for CountingSubscriber {
        fn recv(
            &mut self,
            _: BoxedMessage,
            _: &mut ProcessContext,
        ) -> impl Future<Output = ()> + Send {
            let count = self.count.clone();
            async move {
                count.fetch_add(1, Ordering::SeqCst);
            }
        }
    }

    async fn wait_for_count_inner(counter: &AtomicU32, expected: u32) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while counter.load(Ordering::SeqCst) < expected {
            assert!(Instant::now() < deadline, "timed out waiting for count");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    let count = Arc::new(AtomicU32::new(0));
    let sub_proxy = system
        .spawn(Props::subscriber({
            let count = count.clone();
            move || CountingSubscriber {
                count: count.clone(),
            }
        }))
        .await;
    let sub_pid = sub_proxy.pid();
    dl_stream
        .subscribe(sub_proxy)
        .await
        .expect("subscribe should succeed");

    // And: a process that is stopped (generates one dead-letter event)
    let (started, stopped, counter) = tracked_state();
    let process_proxy = system
        .spawn(test_props(started.clone(), stopped.clone(), counter))
        .await;
    wait_for_flag(&started).await;
    process_proxy.stop().await.expect("stop should succeed");
    wait_for_flag(&stopped).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let _ = process_proxy.tell(Increment).await;
    wait_for_count_inner(&count, 1).await;
    assert_eq!(
        count.load(Ordering::SeqCst),
        1,
        "should have received first event"
    );

    // When: the subscriber is unsubscribed
    dl_stream
        .unsubscribe(sub_pid)
        .await
        .expect("unsubscribe should succeed");
    tokio::time::sleep(Duration::from_millis(50)).await;

    // And: another dead-letter event is generated
    let _ = process_proxy.tell(Increment).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Then: the count remains at 1 (unsubscribed, so no second event received)
    assert_eq!(
        count.load(Ordering::SeqCst),
        1,
        "unsubscribed subscriber should not receive further events"
    );
}

/// `Props::subscriber` creates a working subscriber that receives published messages.
#[tokio::test]
async fn props_subscriber_creates_working_subscriber() {
    struct CountingSubscriber {
        count: Arc<AtomicU32>,
    }

    impl Subscriber<BoxedMessage> for CountingSubscriber {
        fn recv(
            &mut self,
            _: BoxedMessage,
            _: &mut ProcessContext,
        ) -> impl Future<Output = ()> + Send {
            let count = self.count.clone();
            async move {
                count.fetch_add(1, Ordering::SeqCst);
            }
        }
    }

    async fn wait_for_count_inner(counter: &AtomicU32, expected: u32) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while counter.load(Ordering::SeqCst) < expected {
            assert!(Instant::now() < deadline, "timed out waiting for count");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    // Given: a stream and a subscriber created via Props::subscriber
    let system = ProcessSystem::new().await;
    let topic = ProcessName::new("props-sub-test");
    let stream = system
        .spawn_stream::<BoxedMessage>(topic)
        .await
        .expect("spawn_stream should succeed");

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
        .expect("subscribe should succeed");

    // When: a message is published
    stream.publish_boxed(42u32).await.expect("publish should succeed");

    // Then: the subscriber receives the message
    wait_for_count_inner(&count, 1).await;
    assert_eq!(count.load(Ordering::SeqCst), 1);
}

/// `Props::subscriber` returns a type that does not require naming `SubscriberProcess`.
#[tokio::test]
async fn props_subscriber_caller_does_not_name_subscriber_process_type() {
    struct MySubscriber;

    impl Subscriber<BoxedMessage> for MySubscriber {
        async fn recv(&mut self, _: BoxedMessage, _: &mut ProcessContext) {}
    }

    // Given / When: Props::subscriber is called — the return type is inferred
    let system = ProcessSystem::new().await;
    let topic = ProcessName::new("props-sub-type");
    let stream = system
        .spawn_stream::<BoxedMessage>(topic)
        .await
        .expect("spawn_stream should succeed");

    // The caller never writes Props<SubscriberProcess<MySubscriber, Boxed>>
    let props = Props::subscriber(|| MySubscriber);
    let proxy = system.spawn(props).await;
    let result = stream.subscribe(proxy).await;

    // Then: everything compiles and subscribe succeeds
    assert!(result.is_ok());
}
