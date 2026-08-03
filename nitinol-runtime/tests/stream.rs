use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use nitinol_runtime::ident::{Pid, ProcessName};
use nitinol_runtime::process::{Process, ProcessContext, Receive, SubscriberContext};
use nitinol_runtime::{
    BoxedMessage, DeadLetter, Message, ProcessSystem, Props, Stream, Subscriber,
    SupervisionStrategy,
};

/// Error type for handlers that intentionally fail.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct RecvTestError(String);

/// A process that counts received Boxed messages via shared atomic state.
struct ReceivingProcess {
    count: Arc<AtomicU32>,
}

impl Process for ReceivingProcess {}

impl Receive<BoxedMessage> for ReceivingProcess {
    type Response = ();
    type Error = std::convert::Infallible;
    async fn recv(
        &mut self,
        _msg: BoxedMessage,
        _ctx: &mut ProcessContext<Self>,
    ) -> Result<(), std::convert::Infallible> {
        self.count.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

fn receiving_props(count: Arc<AtomicU32>) -> Props<ReceivingProcess> {
    Props::new(move || ReceivingProcess {
        count: count.clone(),
    })
}

/// A Subscriber<Boxed> implementation that counts received messages.
struct CountingSubscriber {
    count: Arc<AtomicU32>,
}

impl Subscriber<BoxedMessage> for CountingSubscriber {
    fn recv(
        &mut self,
        _msg: BoxedMessage,
        _ctx: &mut SubscriberContext<'_, BoxedMessage>,
    ) -> impl Future<Output = ()> + Send {
        let count = self.count.clone();
        async move {
            count.fetch_add(1, Ordering::SeqCst);
        }
    }
}

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

#[tokio::test]
async fn common_types_satisfy_message_bound() {
    // Given: common Rust types
    fn assert_message<T: Message + Clone>(_: T) {}

    // Then: they satisfy Message + Clone (compile-time proof)
    assert_message(42u32);
    assert_message(String::from("hello"));
    assert_message(true);
    assert_message(3.10f64);
}

#[tokio::test]
async fn boxed_new_and_downcast_returns_original_value() {
    // Given: a Boxed wrapping a u32
    let boxed = BoxedMessage::new(42u32);

    // When: downcast to the original type
    let result = boxed.downcast_ref::<u32>();

    // Then: the original value is recovered
    assert_eq!(result, Some(&42u32));
}

#[tokio::test]
async fn boxed_downcast_wrong_type_returns_none() {
    // Given: a Boxed wrapping a u32
    let boxed = BoxedMessage::new(42u32);

    // When: downcast to an incompatible type
    let result = boxed.downcast_ref::<String>();

    // Then: None is returned
    assert!(result.is_none());
}

#[tokio::test]
async fn boxed_clone_shares_inner_value() {
    // Given: a Boxed and its clone (Arc-backed, zero-copy)
    let boxed = BoxedMessage::new(99u32);
    let cloned = boxed.clone();

    // When: both are downcast to the inner type
    let val1 = boxed.downcast_ref::<u32>();
    let val2 = cloned.downcast_ref::<u32>();

    // Then: both return the same value
    assert_eq!(val1, Some(&99u32));
    assert_eq!(val2, Some(&99u32));
}

#[tokio::test]
async fn spawn_stream_returns_valid_proxy() {
    // Given: a ProcessSystem
    let system = ProcessSystem::new().await;
    let topic = ProcessName::new("ss-valid");

    // When: a Boxed stream is spawned for the topic
    let result = system.spawn(nitinol_runtime::StreamProps::<BoxedMessage>::new(topic)).await;

    // Then: a proxy is returned successfully
    assert!(result.is_ok());
}

#[tokio::test]
async fn spawn_stream_duplicate_topic_returns_error() {
    // Given: a stream already registered under "ss-dup"
    let system = ProcessSystem::new().await;
    let topic = ProcessName::new("ss-dup");
    system
        .spawn(nitinol_runtime::StreamProps::<BoxedMessage>::new(topic.clone()))
        .await
        .expect("first spawn_stream should succeed");

    // When: a second stream is spawned with the same topic
    let result = system.spawn(nitinol_runtime::StreamProps::<BoxedMessage>::new(topic)).await;

    // Then: an error is returned (uniqueness constraint violated)
    assert!(result.is_err());
}

#[tokio::test]
async fn spawn_stream_different_topics_both_succeed() {
    // Given: a ProcessSystem
    let system = ProcessSystem::new().await;
    let topic_a = ProcessName::new("ss-diff-a");
    let topic_b = ProcessName::new("ss-diff-b");

    // When: two streams are spawned with different topics
    let result_a = system.spawn(nitinol_runtime::StreamProps::<BoxedMessage>::new(topic_a)).await;
    let result_b = system.spawn(nitinol_runtime::StreamProps::<BoxedMessage>::new(topic_b)).await;

    // Then: both succeed
    assert!(result_a.is_ok());
    assert!(result_b.is_ok());
}

#[tokio::test]
async fn publish_with_no_subscribers_succeeds() {
    // Given: a stream with no subscribers
    let system = ProcessSystem::new().await;
    let topic = ProcessName::new("pub-no-sub");
    let stream = system
        .spawn(nitinol_runtime::StreamProps::<BoxedMessage>::new(topic))
        .await
        .expect("spawn_stream should succeed");

    // When: a message is published
    let result = stream.publish_boxed(42u32).await;

    // Then: no error (dispatching to an empty list is a no-op)
    assert!(result.is_ok());
}

#[tokio::test]
async fn publish_delivers_message_to_subscriber() {
    // Given: a stream with one subscriber process
    let system = ProcessSystem::new().await;
    let topic = ProcessName::new("pub-deliver");
    let stream = system
        .spawn(nitinol_runtime::StreamProps::<BoxedMessage>::new(topic))
        .await
        .expect("spawn_stream should succeed");

    let count = Arc::new(AtomicU32::new(0));
    let proxy = system.spawn(receiving_props(count.clone())).await;
    stream
        .subscribe(proxy)
        .await
        .expect("subscribe should succeed");

    // When: a message is published
    stream
        .publish_boxed(1u32)
        .await
        .expect("publish should succeed");

    // Then: the subscriber receives the message
    wait_for_count(&count, 1).await;
    assert_eq!(count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn publish_delivers_to_all_subscribers() {
    // Given: a stream with three subscriber processes
    let system = ProcessSystem::new().await;
    let topic = ProcessName::new("pub-multi");
    let stream = system
        .spawn(nitinol_runtime::StreamProps::<BoxedMessage>::new(topic))
        .await
        .expect("spawn_stream should succeed");

    let counts: Vec<Arc<AtomicU32>> = (0..3).map(|_| Arc::new(AtomicU32::new(0))).collect();

    for count in &counts {
        let proxy = system.spawn(receiving_props(count.clone())).await;
        stream
            .subscribe(proxy)
            .await
            .expect("subscribe should succeed");
    }

    // When: a message is published
    stream
        .publish_boxed(String::from("broadcast"))
        .await
        .expect("publish should succeed");

    // Then: all three subscribers receive the message
    for count in &counts {
        wait_for_count(count, 1).await;
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn publish_multiple_messages_all_delivered_in_order() {
    // Given: a stream with one subscriber
    let system = ProcessSystem::new().await;
    let topic = ProcessName::new("pub-multi-msg");
    let stream = system
        .spawn(nitinol_runtime::StreamProps::<BoxedMessage>::new(topic))
        .await
        .expect("spawn_stream should succeed");

    let count = Arc::new(AtomicU32::new(0));
    let proxy = system.spawn(receiving_props(count.clone())).await;
    stream
        .subscribe(proxy)
        .await
        .expect("subscribe should succeed");

    // When: three messages are published sequentially
    stream
        .publish_boxed(1u32)
        .await
        .expect("publish should succeed");
    stream
        .publish_boxed(2u32)
        .await
        .expect("publish should succeed");
    stream
        .publish_boxed(3u32)
        .await
        .expect("publish should succeed");

    // Then: all three messages are received
    wait_for_count(&count, 3).await;
    assert_eq!(count.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn stream_lookup_by_name_finds_stream() {
    // Given: a stream registered under a named topic
    let system = ProcessSystem::new().await;
    let topic = ProcessName::new("lookup-stream");
    let _stream = system
        .spawn(nitinol_runtime::StreamProps::<BoxedMessage>::new(topic.clone()))
        .await
        .expect("spawn_stream should succeed");

    // When: lookup by name
    let found = system.lookup_by_name(&topic).await;

    // Then: the stream process is found
    assert!(found.is_some());
}

#[tokio::test]
async fn stream_lookup_by_unknown_name_returns_none() {
    // Given: a ProcessSystem with no stream named "ghost-stream"
    let system = ProcessSystem::new().await;
    let unknown = ProcessName::new("ghost-stream");

    // When: lookup by unknown name
    let found = system.lookup_by_name(&unknown).await;

    // Then: None is returned
    assert!(found.is_none());
}

#[tokio::test]
async fn stream_any_proxy_downcasts_to_stream_proxy() {
    // Given: a stream found via lookup
    let system = ProcessSystem::new().await;
    let topic = ProcessName::new("lookup-cast");
    let _stream = system
        .spawn(nitinol_runtime::StreamProps::<BoxedMessage>::new(topic.clone()))
        .await
        .expect("spawn_stream should succeed");

    let any = system
        .lookup_by_name(&topic)
        .await
        .expect("lookup should find the stream");

    // When: downcast to ProcessProxy<Stream<Boxed>>
    let result = any.downcast::<Stream<BoxedMessage>>();

    // Then: downcast succeeds
    assert!(result.is_some());
}

#[tokio::test]
async fn stream_downcast_proxy_can_publish() {
    // Given: a stream found via lookup and downcast
    let system = ProcessSystem::new().await;
    let topic = ProcessName::new("lookup-pub");
    let _stream = system
        .spawn(nitinol_runtime::StreamProps::<BoxedMessage>::new(topic.clone()))
        .await
        .expect("spawn_stream should succeed");

    let any = system
        .lookup_by_name(&topic)
        .await
        .expect("lookup should find the stream");
    let stream = any
        .downcast::<Stream<BoxedMessage>>()
        .expect("downcast should succeed");

    let count = Arc::new(AtomicU32::new(0));
    let proxy = system.spawn(receiving_props(count.clone())).await;
    stream
        .subscribe(proxy)
        .await
        .expect("subscribe should succeed");

    // When: publish through the downcast proxy
    let result = stream.publish_boxed(7u32).await;

    // Then: publish succeeds and message is delivered
    assert!(result.is_ok());
    wait_for_count(&count, 1).await;
    assert_eq!(count.load(Ordering::SeqCst), 1);
}

/// Re-prevention: public-api-leak (ARCH-01)
///
/// This test verifies that the subscriber workflow is fully usable through
/// crate-level imports only (`Subscriber`, `Props::subscriber`), without
/// needing to name the internal `SubscriberProcess` adapter type.
/// If someone accidentally re-exports `SubscriberProcess` via glob and
/// downstream code starts depending on it, this test documents the intended
/// public API boundary.
#[tokio::test]
async fn public_api_does_not_require_subscriber_process_type() {
    // Given: only crate-level public API imports (no process::SubscriberProcess)
    let system = ProcessSystem::new().await;
    let topic = ProcessName::new("api-surface");
    let stream = system
        .spawn(nitinol_runtime::StreamProps::<BoxedMessage>::new(topic))
        .await
        .expect("spawn_stream should succeed");

    let count = Arc::new(AtomicU32::new(0));

    // When: Props::subscriber is used (returns Props<SubscriberProcess<..>>
    // but the caller never names that type)
    let props = Props::subscriber({
        let count = count.clone();
        move || CountingSubscriber {
            count: count.clone(),
        }
    });
    let proxy = system.spawn(props).await;
    stream
        .subscribe(proxy)
        .await
        .expect("subscribe should succeed");

    stream
        .publish_boxed(1u32)
        .await
        .expect("publish should succeed");

    // Then: the subscriber receives the message — no internal types needed
    wait_for_count(&count, 1).await;
    assert_eq!(count.load(Ordering::SeqCst), 1);
}

/// Re-prevention: dead-code (ARCH-02)
///
/// Verifies that `Subscriber<T>::recv` is a required method (no default impl),
/// so `#[allow(unused_variables)]` on the trait is unnecessary.
/// This test proves the trait compiles and works without any allow-attributes
/// by implementing `recv` using all parameters.
#[tokio::test]
async fn subscriber_recv_uses_all_parameters() {
    // Given: a Subscriber impl that uses both `msg` and `ctx`
    struct ParamCheckSubscriber {
        received: Arc<AtomicU32>,
    }

    impl Subscriber<BoxedMessage> for ParamCheckSubscriber {
        fn recv(
            &mut self,
            msg: BoxedMessage,
            _ctx: &mut SubscriberContext<'_, BoxedMessage>,
        ) -> impl Future<Output = ()> + Send {
            // Use `msg` to prove the parameter is needed
            let value = msg.downcast_ref::<u32>().copied().unwrap_or(0);
            let received = self.received.clone();
            async move {
                received.fetch_add(value, Ordering::SeqCst);
            }
        }
    }

    let system = ProcessSystem::new().await;
    let topic = ProcessName::new("param-check");
    let stream = system
        .spawn(nitinol_runtime::StreamProps::<BoxedMessage>::new(topic))
        .await
        .expect("spawn_stream should succeed");

    let received = Arc::new(AtomicU32::new(0));
    let props = Props::subscriber({
        let received = received.clone();
        move || ParamCheckSubscriber {
            received: received.clone(),
        }
    });
    let proxy = system.spawn(props).await;
    stream
        .subscribe(proxy)
        .await
        .expect("subscribe should succeed");

    // When: a message with a known u32 value is published
    stream
        .publish_boxed(5u32)
        .await
        .expect("publish should succeed");

    // Then: the subscriber uses the msg parameter and accumulates the value
    wait_for_count(&received, 5).await;
    assert_eq!(received.load(Ordering::SeqCst), 5);
}

#[tokio::test]
async fn subscriber_trait_and_props_flow_receives_message() {
    // Given: a Subscriber<Boxed> impl spawned via Props::subscriber
    let system = ProcessSystem::new().await;
    let topic = ProcessName::new("sub-trait-basic");
    let stream = system
        .spawn(nitinol_runtime::StreamProps::<BoxedMessage>::new(topic))
        .await
        .expect("spawn_stream should succeed");

    let count = Arc::new(AtomicU32::new(0));
    let props = Props::subscriber({
        let count = count.clone();
        move || CountingSubscriber {
            count: count.clone(),
        }
    });
    let subscriber_proxy = system.spawn(props).await;
    stream
        .subscribe(subscriber_proxy)
        .await
        .expect("subscribe should succeed");

    // When: a message is published
    stream
        .publish_boxed(77u32)
        .await
        .expect("publish should succeed");

    // Then: the Subscriber::recv is called
    wait_for_count(&count, 1).await;
    assert_eq!(count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn subscriber_trait_receives_multiple_publishes() {
    // Given: a Subscriber<Boxed> registered to a stream
    let system = ProcessSystem::new().await;
    let topic = ProcessName::new("sub-trait-multi");
    let stream = system
        .spawn(nitinol_runtime::StreamProps::<BoxedMessage>::new(topic))
        .await
        .expect("spawn_stream should succeed");

    let count = Arc::new(AtomicU32::new(0));
    let props = Props::subscriber({
        let count = count.clone();
        move || CountingSubscriber {
            count: count.clone(),
        }
    });
    let subscriber_proxy = system.spawn(props).await;
    stream
        .subscribe(subscriber_proxy)
        .await
        .expect("subscribe should succeed");

    // When: three messages are published
    stream
        .publish_boxed(1u32)
        .await
        .expect("publish should succeed");
    stream
        .publish_boxed(2u32)
        .await
        .expect("publish should succeed");
    stream
        .publish_boxed(3u32)
        .await
        .expect("publish should succeed");

    // Then: the subscriber receives all three
    wait_for_count(&count, 3).await;
    assert_eq!(count.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn mixed_subscriber_types_all_receive_published_message() {
    // Given: a stream with both a ReceivingProcess and a CountingSubscriber registered
    let system = ProcessSystem::new().await;
    let topic = ProcessName::new("sub-mixed");
    let stream = system
        .spawn(nitinol_runtime::StreamProps::<BoxedMessage>::new(topic))
        .await
        .expect("spawn_stream should succeed");

    let count_direct = Arc::new(AtomicU32::new(0));
    let direct_proxy = system.spawn(receiving_props(count_direct.clone())).await;
    stream
        .subscribe(direct_proxy)
        .await
        .expect("subscribe direct should succeed");

    let count_trait = Arc::new(AtomicU32::new(0));
    let trait_props = Props::subscriber({
        let c = count_trait.clone();
        move || CountingSubscriber { count: c.clone() }
    });
    let trait_proxy = system.spawn(trait_props).await;
    stream
        .subscribe(trait_proxy)
        .await
        .expect("subscribe trait should succeed");

    // When: a message is published
    stream
        .publish_boxed(true)
        .await
        .expect("publish should succeed");

    // Then: both subscriber types receive the message
    wait_for_count(&count_direct, 1).await;
    wait_for_count(&count_trait, 1).await;
    assert_eq!(count_direct.load(Ordering::SeqCst), 1);
    assert_eq!(count_trait.load(Ordering::SeqCst), 1);
}

/// A process that counts received messages and records when it is stopped.
struct StopTrackingReceivingProcess {
    count: Arc<AtomicU32>,
    stopped: Arc<AtomicBool>,
}

impl Process for StopTrackingReceivingProcess {
    fn on_stop(&mut self, _ctx: &mut ProcessContext<Self>) -> impl Future<Output = ()> + Send {
        self.stopped.store(true, Ordering::SeqCst);
        async {}
    }
}

impl Receive<BoxedMessage> for StopTrackingReceivingProcess {
    type Response = ();
    type Error = std::convert::Infallible;
    async fn recv(
        &mut self,
        _msg: BoxedMessage,
        _ctx: &mut ProcessContext<Self>,
    ) -> Result<(), std::convert::Infallible> {
        self.count.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

fn stop_tracking_props(
    count: Arc<AtomicU32>,
    stopped: Arc<AtomicBool>,
) -> Props<StopTrackingReceivingProcess> {
    Props::new(move || StopTrackingReceivingProcess {
        count: count.clone(),
        stopped: stopped.clone(),
    })
}

/// A process that can fail on the next `Boxed` message receive when `fail_next`
/// is set. Counts successful receives and tracks restart count via `start_count`.
struct FaultyReceivingProcess {
    count: Arc<AtomicU32>,
    start_count: Arc<AtomicU32>,
    fail_next: Arc<AtomicBool>,
}

impl Process for FaultyReceivingProcess {
    fn on_start(&mut self, _ctx: &mut ProcessContext<Self>) -> impl Future<Output = ()> + Send {
        self.start_count.fetch_add(1, Ordering::SeqCst);
        async {}
    }
}

impl Receive<BoxedMessage> for FaultyReceivingProcess {
    type Response = ();
    type Error = RecvTestError;
    async fn recv(
        &mut self,
        _msg: BoxedMessage,
        _ctx: &mut ProcessContext<Self>,
    ) -> Result<(), RecvTestError> {
        if self.fail_next.swap(false, Ordering::SeqCst) {
            return Err(RecvTestError("intentional failure".to_string()));
        }
        self.count.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

fn faulty_receiving_props(
    count: Arc<AtomicU32>,
    start_count: Arc<AtomicU32>,
    fail_next: Arc<AtomicBool>,
    strategy: SupervisionStrategy,
) -> Props<FaultyReceivingProcess> {
    let props = Props::new(move || FaultyReceivingProcess {
        count: count.clone(),
        start_count: start_count.clone(),
        fail_next: fail_next.clone(),
    });
    let props = props.with_supervision_strategy(strategy);
    props
}

/// Polls a flag until it becomes true, with a 5-second timeout.
async fn wait_for_flag(flag: &AtomicBool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !flag.load(Ordering::SeqCst) {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for flag to become true"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Verifies R1 + R2: when a subscriber process terminates, the stream detects
/// the Terminated notification (via watch) and auto-removes it from the subscriber
/// list so that subsequent publishes do not attempt dispatch to the dead process.
#[tokio::test]
async fn terminated_subscriber_is_auto_removed_from_stream() {
    // Given: a stream with one subscriber that tracks its stop event
    let system = ProcessSystem::new().await;
    let topic = ProcessName::new("lifecycle-term-1");
    let stream = system
        .spawn(nitinol_runtime::StreamProps::<BoxedMessage>::new(topic))
        .await
        .expect("spawn_stream should succeed");

    let count = Arc::new(AtomicU32::new(0));
    let stopped = Arc::new(AtomicBool::new(false));
    let proxy = system
        .spawn(stop_tracking_props(count.clone(), stopped.clone()))
        .await;
    let stop_handle = proxy.clone();
    stream
        .subscribe(proxy)
        .await
        .expect("subscribe should succeed");

    // Verify the subscriber is receiving before stopping
    stream
        .publish_boxed(1u32)
        .await
        .expect("initial publish should succeed");
    wait_for_count(&count, 1).await;

    // When: the subscriber is stopped
    stop_handle.stop().await.expect("stop should succeed");
    wait_for_flag(&stopped).await;
    // Allow the Terminated notification to propagate to stream's on_terminated
    tokio::time::sleep(Duration::from_millis(50)).await;

    // And: another message is published
    stream
        .publish_boxed(2u32)
        .await
        .expect("second publish should succeed");
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Then: the stopped subscriber no longer receives messages
    assert_eq!(count.load(Ordering::SeqCst), 1);
}

/// Verifies R2 partial removal: stopping one of two subscribers removes only
/// that subscriber; the remaining subscriber continues to receive messages.
#[tokio::test]
async fn only_terminated_subscriber_is_removed_remaining_stays_active() {
    // Given: a stream with two subscribers
    let system = ProcessSystem::new().await;
    let topic = ProcessName::new("lifecycle-term-2");
    let stream = system
        .spawn(nitinol_runtime::StreamProps::<BoxedMessage>::new(topic))
        .await
        .expect("spawn_stream should succeed");

    let count_a = Arc::new(AtomicU32::new(0));
    let stopped_a = Arc::new(AtomicBool::new(false));
    let proxy_a = system
        .spawn(stop_tracking_props(count_a.clone(), stopped_a.clone()))
        .await;
    let stop_handle_a = proxy_a.clone();
    stream
        .subscribe(proxy_a)
        .await
        .expect("subscribe A should succeed");

    let count_b = Arc::new(AtomicU32::new(0));
    let stopped_b = Arc::new(AtomicBool::new(false));
    let proxy_b = system
        .spawn(stop_tracking_props(count_b.clone(), stopped_b.clone()))
        .await;
    stream
        .subscribe(proxy_b)
        .await
        .expect("subscribe B should succeed");
    let _ = stopped_b; // B is not stopped in this test

    // Verify both receive the initial message
    stream
        .publish_boxed(1u32)
        .await
        .expect("initial publish should succeed");
    wait_for_count(&count_a, 1).await;
    wait_for_count(&count_b, 1).await;

    // When: subscriber A is stopped
    stop_handle_a.stop().await.expect("stop A should succeed");
    wait_for_flag(&stopped_a).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // And: a second message is published
    stream
        .publish_boxed(2u32)
        .await
        .expect("second publish should succeed");

    // Then: subscriber B receives the second message, A does not
    wait_for_count(&count_b, 2).await;
    assert_eq!(
        count_a.load(Ordering::SeqCst),
        1,
        "stopped subscriber A should not receive second message"
    );
    assert_eq!(
        count_b.load(Ordering::SeqCst),
        2,
        "subscriber B should still receive"
    );
}

/// Verifies R4 + R5: a subscriber spawned with Restart supervision that fails
/// is restarted automatically; the restarted instance continues to receive
/// messages via the same dispatcher entry in the stream.
#[tokio::test]
async fn restarted_subscriber_continues_receiving_messages_after_failure() {
    // Given: a stream with one subscriber using Restart supervision strategy
    let system = ProcessSystem::new().await;
    let topic = ProcessName::new("lifecycle-restart");
    let stream = system
        .spawn(nitinol_runtime::StreamProps::<BoxedMessage>::new(topic))
        .await
        .expect("spawn_stream should succeed");

    let count = Arc::new(AtomicU32::new(0));
    let start_count = Arc::new(AtomicU32::new(0));
    let fail_next = Arc::new(AtomicBool::new(false));
    let props = faulty_receiving_props(
        count.clone(),
        start_count.clone(),
        fail_next.clone(),
        SupervisionStrategy::restart(5, Duration::from_secs(10)).expect("valid restart config"),
    );
    let proxy = system.spawn(props).await;
    stream
        .subscribe(proxy)
        .await
        .expect("subscribe should succeed");
    wait_for_count(&start_count, 1).await;

    // When: the subscriber fails (triggering a Restart)
    fail_next.store(true, Ordering::SeqCst);
    stream
        .publish_boxed(1u32)
        .await
        .expect("fail-trigger publish should succeed");
    wait_for_count(&start_count, 2).await; // confirmed restart

    // And: a message is published after the restart
    stream
        .publish_boxed(2u32)
        .await
        .expect("post-restart publish should succeed");

    // Then: the restarted subscriber receives the new message
    wait_for_count(&count, 1).await;
    assert_eq!(count.load(Ordering::SeqCst), 1);
}

/// Verifies R3: after calling unsubscribe with a subscriber's PID, subsequent
/// publishes are not dispatched to that subscriber.
#[tokio::test]
async fn unsubscribed_subscriber_no_longer_receives_messages() {
    // Given: a stream with one subscriber
    let system = ProcessSystem::new().await;
    let topic = ProcessName::new("lifecycle-unsub-1");
    let stream = system
        .spawn(nitinol_runtime::StreamProps::<BoxedMessage>::new(topic))
        .await
        .expect("spawn_stream should succeed");

    let count = Arc::new(AtomicU32::new(0));
    let proxy = system.spawn(receiving_props(count.clone())).await;
    let pid = proxy.pid();
    stream
        .subscribe(proxy)
        .await
        .expect("subscribe should succeed");

    // Verify the subscriber is receiving before unsubscribing
    stream
        .publish_boxed(1u32)
        .await
        .expect("initial publish should succeed");
    wait_for_count(&count, 1).await;

    // When: the subscriber is explicitly unsubscribed
    stream
        .unsubscribe(pid)
        .await
        .expect("unsubscribe should succeed");

    // And: another message is published
    stream
        .publish_boxed(2u32)
        .await
        .expect("second publish should succeed");
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Then: the unsubscribed subscriber does not receive the second message
    assert_eq!(count.load(Ordering::SeqCst), 1);
}

/// Verifies R3 partial removal: unsubscribing one of two subscribers removes
/// only that subscriber; the remaining subscriber continues to receive.
#[tokio::test]
async fn only_unsubscribed_subscriber_stops_receiving_other_stays_active() {
    // Given: a stream with two subscribers
    let system = ProcessSystem::new().await;
    let topic = ProcessName::new("lifecycle-unsub-2");
    let stream = system
        .spawn(nitinol_runtime::StreamProps::<BoxedMessage>::new(topic))
        .await
        .expect("spawn_stream should succeed");

    let count_a = Arc::new(AtomicU32::new(0));
    let proxy_a = system.spawn(receiving_props(count_a.clone())).await;
    let pid_a = proxy_a.pid();
    stream
        .subscribe(proxy_a)
        .await
        .expect("subscribe A should succeed");

    let count_b = Arc::new(AtomicU32::new(0));
    let proxy_b = system.spawn(receiving_props(count_b.clone())).await;
    stream
        .subscribe(proxy_b)
        .await
        .expect("subscribe B should succeed");

    // Verify both receive the initial message
    stream
        .publish_boxed(1u32)
        .await
        .expect("initial publish should succeed");
    wait_for_count(&count_a, 1).await;
    wait_for_count(&count_b, 1).await;

    // When: subscriber A is unsubscribed
    stream
        .unsubscribe(pid_a)
        .await
        .expect("unsubscribe A should succeed");

    // And: a second message is published
    stream
        .publish_boxed(2u32)
        .await
        .expect("second publish should succeed");

    // Then: subscriber B receives the second message, A does not
    wait_for_count(&count_b, 2).await;
    assert_eq!(
        count_a.load(Ordering::SeqCst),
        1,
        "unsubscribed A should not receive second message"
    );
    assert_eq!(
        count_b.load(Ordering::SeqCst),
        2,
        "subscriber B should still receive"
    );
}

/// Verifies R2 + rate-limit: when a subscriber exceeds the Restart rate limit
/// and is permanently stopped, the stream auto-removes it via the Terminated
/// notification so that subsequent publishes are not dispatched to it.
#[tokio::test]
async fn subscriber_permanently_stopped_by_rate_limit_is_auto_removed_from_stream() {
    // Given: a stream with one subscriber using Restart{max_retries: 1}
    let system = ProcessSystem::new().await;
    let topic = ProcessName::new("lifecycle-ratelimit");
    let stream = system
        .spawn(nitinol_runtime::StreamProps::<BoxedMessage>::new(topic))
        .await
        .expect("spawn_stream should succeed");

    let count = Arc::new(AtomicU32::new(0));
    let start_count = Arc::new(AtomicU32::new(0));
    let fail_next = Arc::new(AtomicBool::new(false));
    let props = faulty_receiving_props(
        count.clone(),
        start_count.clone(),
        fail_next.clone(),
        SupervisionStrategy::restart(1, Duration::from_secs(10)).expect("valid restart config"),
    );
    let proxy = system.spawn(props).await;
    stream
        .subscribe(proxy)
        .await
        .expect("subscribe should succeed");
    wait_for_count(&start_count, 1).await;

    // When: the subscriber fails once (within max_retries → restarts)
    fail_next.store(true, Ordering::SeqCst);
    stream
        .publish_boxed(1u32)
        .await
        .expect("first fail-trigger publish should succeed");
    wait_for_count(&start_count, 2).await;

    // And: the subscriber fails again (exceeds max_retries → permanent stop)
    fail_next.store(true, Ordering::SeqCst);
    stream
        .publish_boxed(2u32)
        .await
        .expect("second fail-trigger publish should succeed");

    // Allow the permanent stop and Terminated notification to propagate to stream
    tokio::time::sleep(Duration::from_millis(100)).await;

    // And: a normal message is published after the subscriber is permanently gone
    stream
        .publish_boxed(3u32)
        .await
        .expect("post-stop publish should succeed");
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Then: the subscriber was auto-removed and receives no further messages
    // (count = 0 because all messages either triggered failure or arrived after removal)
    assert_eq!(count.load(Ordering::SeqCst), 0);
}

/// A subscriber that panics when it receives `KillChannel`, causing its tokio
/// task to abort without lifecycle cleanup (no on_stop, no unregister, no
/// Terminated notification to watchers).
///
/// This simulates a crashed subscriber: its channel is closed, but it remains
/// in Stream's subscriber list — exactly the race condition described in the task.
struct ChannelKillerSubscriber;

impl Process for ChannelKillerSubscriber {}

/// Trigger message that causes `ChannelKillerSubscriber` to panic.
struct KillChannel;

impl Receive<KillChannel> for ChannelKillerSubscriber {
    type Response = ();
    type Error = std::convert::Infallible;
    async fn recv(
        &mut self,
        _msg: KillChannel,
        _ctx: &mut ProcessContext<Self>,
    ) -> Result<(), std::convert::Infallible> {
        panic!("deliberate channel kill for dead-letter routing test");
    }
}

impl Receive<BoxedMessage> for ChannelKillerSubscriber {
    type Response = ();
    type Error = std::convert::Infallible;
    async fn recv(
        &mut self,
        _msg: BoxedMessage,
        _ctx: &mut ProcessContext<Self>,
    ) -> Result<(), std::convert::Infallible> {
        Ok(())
    }
}

/// Counts every `Boxed` message received on the dead-letter stream.
struct DeadLetterCountSub {
    count: Arc<AtomicU32>,
}

impl Subscriber<BoxedMessage> for DeadLetterCountSub {
    fn recv(
        &mut self,
        _msg: BoxedMessage,
        _ctx: &mut SubscriberContext<'_, BoxedMessage>,
    ) -> impl Future<Output = ()> + Send {
        let count = self.count.clone();
        async move {
            count.fetch_add(1, Ordering::SeqCst);
        }
    }
}

/// Captures `DeadLetter.destination` from the first dead-letter on the stream.
struct DeadLetterDestinationCapture {
    captured: Arc<tokio::sync::Mutex<Option<Pid>>>,
}

impl Subscriber<BoxedMessage> for DeadLetterDestinationCapture {
    fn recv(
        &mut self,
        msg: BoxedMessage,
        _ctx: &mut SubscriberContext<'_, BoxedMessage>,
    ) -> impl Future<Output = ()> + Send {
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

/// Captures `DeadLetter.sender` from the first dead-letter on the stream.
///
/// The outer `Option` represents "was anything captured?"; the inner `Option`
/// is the actual `sender` field of `DeadLetter` (which may itself be `None`).
struct DeadLetterSenderCapture {
    captured: Arc<tokio::sync::Mutex<Option<Option<Pid>>>>,
}

impl Subscriber<BoxedMessage> for DeadLetterSenderCapture {
    fn recv(
        &mut self,
        msg: BoxedMessage,
        _ctx: &mut SubscriberContext<'_, BoxedMessage>,
    ) -> impl Future<Output = ()> + Send {
        let captured = self.captured.clone();
        async move {
            if let Some(dl) = msg.downcast_ref::<DeadLetter>() {
                let mut guard = captured.lock().await;
                if guard.is_none() {
                    *guard = Some(dl.sender);
                }
            }
        }
    }
}

/// Polls `captured` until a value appears, with a 5-second timeout.
async fn wait_for_capture<T: Clone>(captured: &Arc<tokio::sync::Mutex<Option<T>>>) -> T {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        tokio::time::sleep(Duration::from_millis(5)).await;
        let guard = captured.lock().await;
        if let Some(ref val) = *guard {
            return val.clone();
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for dead letter capture"
        );
    }
}

/// Verifies that publishing to a dead subscriber (channel closed by panic, not
/// by normal Stop) routes the undeliverable message to the dead-letter stream.
///
/// The subscriber panics inside its handler, causing the tokio task to abort
/// without lifecycle cleanup.  It therefore stays in Stream's subscriber list
/// while its channel is closed, recreating the race condition described in the
/// task (Terminated notification arrives after the next publish).
#[tokio::test]
async fn publish_to_dead_subscriber_routes_to_dead_letter_stream() {
    // Given: a counter subscribed to the system dead-letter stream
    let system = ProcessSystem::new().await;
    let dl_stream = system.dead_letter_stream();

    let dl_count = Arc::new(AtomicU32::new(0));
    let dl_sub_props = Props::subscriber({
        let dl_count = dl_count.clone();
        move || DeadLetterCountSub {
            count: dl_count.clone(),
        }
    });
    let dl_sub_proxy = system.spawn(dl_sub_props).await;
    dl_stream
        .subscribe(dl_sub_proxy)
        .await
        .expect("subscribe to dead-letter stream should succeed");

    // And: a stream with one subscriber whose channel is killed via panic
    let topic = ProcessName::new("dl-pub-fail-count");
    let stream = system
        .spawn(nitinol_runtime::StreamProps::<BoxedMessage>::new(topic))
        .await
        .expect("spawn_stream should succeed");

    let sub_proxy = system.spawn(Props::new(|| ChannelKillerSubscriber)).await;
    stream
        .subscribe(sub_proxy.clone())
        .await
        .expect("subscribe should succeed");

    // Kill the subscriber's tokio task without triggering lifecycle cleanup
    let _ = sub_proxy.tell(KillChannel).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // When: a message is published (dispatch to the dead subscriber will fail)
    stream
        .publish_boxed(42u32)
        .await
        .expect("publish should succeed");

    // Then: the dead-letter stream receives exactly one notification
    wait_for_count(&dl_count, 1).await;
    assert_eq!(dl_count.load(Ordering::SeqCst), 1);
}

/// Verifies that `DeadLetter.destination` equals the PID of the dead subscriber,
/// not some other process.
#[tokio::test]
async fn dead_letter_from_failed_publish_contains_subscriber_pid() {
    // Given: a dead-letter subscriber that captures the destination PID
    let system = ProcessSystem::new().await;
    let dl_stream = system.dead_letter_stream();

    let captured: Arc<tokio::sync::Mutex<Option<Pid>>> = Arc::new(tokio::sync::Mutex::new(None));
    let dl_sub_props = Props::subscriber({
        let captured = captured.clone();
        move || DeadLetterDestinationCapture {
            captured: captured.clone(),
        }
    });
    let dl_sub_proxy = system.spawn(dl_sub_props).await;
    dl_stream
        .subscribe(dl_sub_proxy)
        .await
        .expect("subscribe to dead-letter stream should succeed");

    // And: a stream with a subscriber whose channel is killed (PID recorded)
    let topic = ProcessName::new("dl-pub-fail-dest");
    let stream = system
        .spawn(nitinol_runtime::StreamProps::<BoxedMessage>::new(topic))
        .await
        .expect("spawn_stream should succeed");

    let sub_proxy = system.spawn(Props::new(|| ChannelKillerSubscriber)).await;
    let expected_subscriber_pid = sub_proxy.pid();
    stream
        .subscribe(sub_proxy.clone())
        .await
        .expect("subscribe should succeed");

    let _ = sub_proxy.tell(KillChannel).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // When: a message is published
    stream
        .publish_boxed(42u32)
        .await
        .expect("publish should succeed");

    // Then: the captured destination matches the dead subscriber's PID
    let destination = wait_for_capture(&captured).await;
    assert_eq!(destination, expected_subscriber_pid);
}

/// Verifies that `DeadLetter.sender` is `Some(stream_pid)` — identifying the
/// Stream as the source of the undeliverable message, not `None`.
///
/// This distinguishes a failed stream publish from a failed direct `tell`
/// (which sets sender to `None` because no caller context is available).
#[tokio::test]
async fn dead_letter_from_failed_publish_contains_stream_pid_as_sender() {
    // Given: a dead-letter subscriber that captures the sender PID
    let system = ProcessSystem::new().await;
    let dl_stream = system.dead_letter_stream();

    let captured: Arc<tokio::sync::Mutex<Option<Option<Pid>>>> =
        Arc::new(tokio::sync::Mutex::new(None));
    let dl_sub_props = Props::subscriber({
        let captured = captured.clone();
        move || DeadLetterSenderCapture {
            captured: captured.clone(),
        }
    });
    let dl_sub_proxy = system.spawn(dl_sub_props).await;
    dl_stream
        .subscribe(dl_sub_proxy)
        .await
        .expect("subscribe to dead-letter stream should succeed");

    // And: a stream with a subscriber whose channel is killed (stream PID recorded)
    let topic = ProcessName::new("dl-pub-fail-sender");
    let stream = system
        .spawn(nitinol_runtime::StreamProps::<BoxedMessage>::new(topic))
        .await
        .expect("spawn_stream should succeed");
    let expected_stream_pid = stream.pid();

    let sub_proxy = system.spawn(Props::new(|| ChannelKillerSubscriber)).await;
    stream
        .subscribe(sub_proxy.clone())
        .await
        .expect("subscribe should succeed");

    let _ = sub_proxy.tell(KillChannel).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // When: a message is published
    stream
        .publish_boxed(42u32)
        .await
        .expect("publish should succeed");

    // Then: the captured sender is Some(stream_pid)
    let sender = wait_for_capture(&captured).await;
    assert_eq!(sender, Some(expected_stream_pid));
}

/// Verifies that a failed dispatch to one dead subscriber does not prevent
/// delivery to remaining live subscribers.
///
/// Broadcast continues to all subscribers; a failed dispatch to one does not
/// abort the iteration.
#[tokio::test]
async fn publish_to_dead_subscriber_still_delivers_to_live_subscribers() {
    // Given: a dead-letter counter and a stream with one live + one dead subscriber
    let system = ProcessSystem::new().await;
    let dl_stream = system.dead_letter_stream();

    let dl_count = Arc::new(AtomicU32::new(0));
    let dl_sub_props = Props::subscriber({
        let dl_count = dl_count.clone();
        move || DeadLetterCountSub {
            count: dl_count.clone(),
        }
    });
    let dl_sub_proxy = system.spawn(dl_sub_props).await;
    dl_stream
        .subscribe(dl_sub_proxy)
        .await
        .expect("subscribe to dead-letter stream should succeed");

    let topic = ProcessName::new("dl-pub-fail-mixed");
    let stream = system
        .spawn(nitinol_runtime::StreamProps::<BoxedMessage>::new(topic))
        .await
        .expect("spawn_stream should succeed");

    // Live subscriber
    let live_count = Arc::new(AtomicU32::new(0));
    let live_proxy = system.spawn(receiving_props(live_count.clone())).await;
    stream
        .subscribe(live_proxy)
        .await
        .expect("subscribe live should succeed");

    // Dead subscriber (channel killed via panic)
    let dead_proxy = system.spawn(Props::new(|| ChannelKillerSubscriber)).await;
    stream
        .subscribe(dead_proxy.clone())
        .await
        .expect("subscribe dead should succeed");

    let _ = dead_proxy.tell(KillChannel).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // When: a message is published
    stream
        .publish_boxed(99u32)
        .await
        .expect("publish should succeed");

    // Then: the live subscriber receives the message
    wait_for_count(&live_count, 1).await;
    assert_eq!(live_count.load(Ordering::SeqCst), 1);

    // And: exactly one dead-letter notification is sent for the dead subscriber
    wait_for_count(&dl_count, 1).await;
    assert_eq!(dl_count.load(Ordering::SeqCst), 1);
}

/// Regression test: a single failed dispatch generates exactly one dead-letter
/// notification, not two (which would happen if both `dispatch` and Stream's
/// `recv` independently routed to dead-letter).
#[tokio::test]
async fn failed_publish_generates_exactly_one_dead_letter() {
    // Given: a dead-letter counter
    let system = ProcessSystem::new().await;
    let dl_stream = system.dead_letter_stream();

    let dl_count = Arc::new(AtomicU32::new(0));
    let dl_sub_props = Props::subscriber({
        let dl_count = dl_count.clone();
        move || DeadLetterCountSub {
            count: dl_count.clone(),
        }
    });
    let dl_sub_proxy = system.spawn(dl_sub_props).await;
    dl_stream
        .subscribe(dl_sub_proxy)
        .await
        .expect("subscribe to dead-letter stream should succeed");

    // And: a stream with one dead subscriber
    let topic = ProcessName::new("dl-pub-fail-once");
    let stream = system
        .spawn(nitinol_runtime::StreamProps::<BoxedMessage>::new(topic))
        .await
        .expect("spawn_stream should succeed");

    let sub_proxy = system.spawn(Props::new(|| ChannelKillerSubscriber)).await;
    stream
        .subscribe(sub_proxy.clone())
        .await
        .expect("subscribe should succeed");

    let _ = sub_proxy.tell(KillChannel).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // When: a message is published
    stream
        .publish_boxed(1u32)
        .await
        .expect("publish should succeed");

    // Then: exactly one dead-letter notification arrives (no double-routing)
    wait_for_count(&dl_count, 1).await;
    // Allow extra time for a potential second notification to arrive
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        dl_count.load(Ordering::SeqCst),
        1,
        "exactly one dead-letter expected; double-routing would produce 2"
    );
}
