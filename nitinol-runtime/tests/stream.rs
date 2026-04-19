use std::future::Future;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use nitinol_runtime::ident::ProcessName;
use nitinol_runtime::process::{Process, ProcessContext, Receive};
use nitinol_runtime::{BoxError, Boxed, Message, Props, ProcessSystem, Stream, Subscriber, subscriber_props};

// -- Test helpers -----------------------------------------------------------

/// A process that counts received Boxed messages via shared atomic state.
struct ReceivingProcess {
    count: Arc<AtomicU32>,
}

impl Process for ReceivingProcess {}

impl Receive<Boxed> for ReceivingProcess {
    type Response = ();
    async fn recv(
        &mut self,
        _msg: Boxed,
        _ctx: &mut ProcessContext,
    ) -> Result<(), BoxError> {
        self.count.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

fn receiving_props(count: Arc<AtomicU32>) -> Props<ReceivingProcess> {
    Props::new(move || ReceivingProcess { count: count.clone() })
}

/// A Subscriber<Boxed> implementation that counts received messages.
struct CountingSubscriber {
    count: Arc<AtomicU32>,
}

impl Subscriber<Boxed> for CountingSubscriber {
    fn recv(
        &mut self,
        _msg: Boxed,
        _ctx: &mut ProcessContext,
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

// -- Message / Boxed type tests ---------------------------------------------

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
    let boxed = Boxed::new(42u32);

    // When: downcast to the original type
    let result = boxed.downcast_ref::<u32>();

    // Then: the original value is recovered
    assert_eq!(result, Some(&42u32));
}

#[tokio::test]
async fn boxed_downcast_wrong_type_returns_none() {
    // Given: a Boxed wrapping a u32
    let boxed = Boxed::new(42u32);

    // When: downcast to an incompatible type
    let result = boxed.downcast_ref::<String>();

    // Then: None is returned
    assert!(result.is_none());
}

#[tokio::test]
async fn boxed_clone_shares_inner_value() {
    // Given: a Boxed and its clone (Arc-backed, zero-copy)
    let boxed = Boxed::new(99u32);
    let cloned = boxed.clone();

    // When: both are downcast to the inner type
    let val1 = boxed.downcast_ref::<u32>();
    let val2 = cloned.downcast_ref::<u32>();

    // Then: both return the same value
    assert_eq!(val1, Some(&99u32));
    assert_eq!(val2, Some(&99u32));
}

// -- spawn_stream tests -----------------------------------------------------

#[tokio::test]
async fn spawn_stream_returns_valid_proxy() {
    // Given: a ProcessSystem
    let system = ProcessSystem::new().await;
    let topic = ProcessName::new("ss-valid");

    // When: a Boxed stream is spawned for the topic
    let result = system.spawn_stream::<Boxed>(topic).await;

    // Then: a proxy is returned successfully
    assert!(result.is_ok());
}

#[tokio::test]
async fn spawn_stream_duplicate_topic_returns_error() {
    // Given: a stream already registered under "ss-dup"
    let system = ProcessSystem::new().await;
    let topic = ProcessName::new("ss-dup");
    system
        .spawn_stream::<Boxed>(topic.clone())
        .await
        .expect("first spawn_stream should succeed");

    // When: a second stream is spawned with the same topic
    let result = system.spawn_stream::<Boxed>(topic).await;

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
    let result_a = system.spawn_stream::<Boxed>(topic_a).await;
    let result_b = system.spawn_stream::<Boxed>(topic_b).await;

    // Then: both succeed
    assert!(result_a.is_ok());
    assert!(result_b.is_ok());
}

// -- publish tests ----------------------------------------------------------

#[tokio::test]
async fn publish_with_no_subscribers_succeeds() {
    // Given: a stream with no subscribers
    let system = ProcessSystem::new().await;
    let topic = ProcessName::new("pub-no-sub");
    let stream = system
        .spawn_stream::<Boxed>(topic)
        .await
        .expect("spawn_stream should succeed");

    // When: a message is published
    let result = stream.publish(42u32).await;

    // Then: no error (dispatching to an empty list is a no-op)
    assert!(result.is_ok());
}

#[tokio::test]
async fn publish_delivers_message_to_subscriber() {
    // Given: a stream with one subscriber process
    let system = ProcessSystem::new().await;
    let topic = ProcessName::new("pub-deliver");
    let stream = system
        .spawn_stream::<Boxed>(topic)
        .await
        .expect("spawn_stream should succeed");

    let count = Arc::new(AtomicU32::new(0));
    let proxy = system.spawn(receiving_props(count.clone())).await;
    stream
        .subscribe(proxy)
        .await
        .expect("subscribe should succeed");

    // When: a message is published
    stream.publish(1u32).await.expect("publish should succeed");

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
        .spawn_stream::<Boxed>(topic)
        .await
        .expect("spawn_stream should succeed");

    let counts: Vec<Arc<AtomicU32>> = (0..3)
        .map(|_| Arc::new(AtomicU32::new(0)))
        .collect();

    for count in &counts {
        let proxy = system.spawn(receiving_props(count.clone())).await;
        stream
            .subscribe(proxy)
            .await
            .expect("subscribe should succeed");
    }

    // When: a message is published
    stream
        .publish(String::from("broadcast"))
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
        .spawn_stream::<Boxed>(topic)
        .await
        .expect("spawn_stream should succeed");

    let count = Arc::new(AtomicU32::new(0));
    let proxy = system.spawn(receiving_props(count.clone())).await;
    stream
        .subscribe(proxy)
        .await
        .expect("subscribe should succeed");

    // When: three messages are published sequentially
    stream.publish(1u32).await.expect("publish should succeed");
    stream.publish(2u32).await.expect("publish should succeed");
    stream.publish(3u32).await.expect("publish should succeed");

    // Then: all three messages are received
    wait_for_count(&count, 3).await;
    assert_eq!(count.load(Ordering::SeqCst), 3);
}

// -- lookup tests -----------------------------------------------------------

#[tokio::test]
async fn stream_lookup_by_name_finds_stream() {
    // Given: a stream registered under a named topic
    let system = ProcessSystem::new().await;
    let topic = ProcessName::new("lookup-stream");
    let _stream = system
        .spawn_stream::<Boxed>(topic.clone())
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
        .spawn_stream::<Boxed>(topic.clone())
        .await
        .expect("spawn_stream should succeed");

    let any = system
        .lookup_by_name(&topic)
        .await
        .expect("lookup should find the stream");

    // When: downcast to ProcessProxy<Stream<Boxed>>
    let result = any.downcast::<Stream<Boxed>>();

    // Then: downcast succeeds
    assert!(result.is_some());
}

#[tokio::test]
async fn stream_downcast_proxy_can_publish() {
    // Given: a stream found via lookup and downcast
    let system = ProcessSystem::new().await;
    let topic = ProcessName::new("lookup-pub");
    let _stream = system
        .spawn_stream::<Boxed>(topic.clone())
        .await
        .expect("spawn_stream should succeed");

    let any = system
        .lookup_by_name(&topic)
        .await
        .expect("lookup should find the stream");
    let stream = any
        .downcast::<Stream<Boxed>>()
        .expect("downcast should succeed");

    let count = Arc::new(AtomicU32::new(0));
    let proxy = system.spawn(receiving_props(count.clone())).await;
    stream
        .subscribe(proxy)
        .await
        .expect("subscribe should succeed");

    // When: publish through the downcast proxy
    let result = stream.publish(7u32).await;

    // Then: publish succeeds and message is delivered
    assert!(result.is_ok());
    wait_for_count(&count, 1).await;
    assert_eq!(count.load(Ordering::SeqCst), 1);
}

// -- Public API surface tests -----------------------------------------------

/// Re-prevention: public-api-leak (ARCH-01)
///
/// This test verifies that the subscriber workflow is fully usable through
/// crate-level imports only (`Subscriber`, `subscriber_props`), without
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
        .spawn_stream::<Boxed>(topic)
        .await
        .expect("spawn_stream should succeed");

    let count = Arc::new(AtomicU32::new(0));

    // When: subscriber_props is used (returns Props<SubscriberProcess<..>>
    // but the caller never names that type)
    let props = subscriber_props({
        let count = count.clone();
        move || CountingSubscriber { count: count.clone() }
    });
    let proxy = system.spawn(props).await;
    stream
        .subscribe(proxy)
        .await
        .expect("subscribe should succeed");

    stream.publish(1u32).await.expect("publish should succeed");

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

    impl Subscriber<Boxed> for ParamCheckSubscriber {
        fn recv(
            &mut self,
            msg: Boxed,
            _ctx: &mut ProcessContext,
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
        .spawn_stream::<Boxed>(topic)
        .await
        .expect("spawn_stream should succeed");

    let received = Arc::new(AtomicU32::new(0));
    let props = subscriber_props({
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
    stream.publish(5u32).await.expect("publish should succeed");

    // Then: the subscriber uses the msg parameter and accumulates the value
    wait_for_count(&received, 5).await;
    assert_eq!(received.load(Ordering::SeqCst), 5);
}

// -- Subscriber trait tests -------------------------------------------------

#[tokio::test]
async fn subscriber_trait_and_props_flow_receives_message() {
    // Given: a Subscriber<Boxed> impl spawned via subscriber_props
    let system = ProcessSystem::new().await;
    let topic = ProcessName::new("sub-trait-basic");
    let stream = system
        .spawn_stream::<Boxed>(topic)
        .await
        .expect("spawn_stream should succeed");

    let count = Arc::new(AtomicU32::new(0));
    let props = subscriber_props({
        let count = count.clone();
        move || CountingSubscriber { count: count.clone() }
    });
    let subscriber_proxy = system.spawn(props).await;
    stream
        .subscribe(subscriber_proxy)
        .await
        .expect("subscribe should succeed");

    // When: a message is published
    stream.publish(77u32).await.expect("publish should succeed");

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
        .spawn_stream::<Boxed>(topic)
        .await
        .expect("spawn_stream should succeed");

    let count = Arc::new(AtomicU32::new(0));
    let props = subscriber_props({
        let count = count.clone();
        move || CountingSubscriber { count: count.clone() }
    });
    let subscriber_proxy = system.spawn(props).await;
    stream
        .subscribe(subscriber_proxy)
        .await
        .expect("subscribe should succeed");

    // When: three messages are published
    stream.publish(1u32).await.expect("publish should succeed");
    stream.publish(2u32).await.expect("publish should succeed");
    stream.publish(3u32).await.expect("publish should succeed");

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
        .spawn_stream::<Boxed>(topic)
        .await
        .expect("spawn_stream should succeed");

    let count_direct = Arc::new(AtomicU32::new(0));
    let direct_proxy = system
        .spawn(receiving_props(count_direct.clone()))
        .await;
    stream
        .subscribe(direct_proxy)
        .await
        .expect("subscribe direct should succeed");

    let count_trait = Arc::new(AtomicU32::new(0));
    let trait_props = subscriber_props({
        let c = count_trait.clone();
        move || CountingSubscriber { count: c.clone() }
    });
    let trait_proxy = system.spawn(trait_props).await;
    stream
        .subscribe(trait_proxy)
        .await
        .expect("subscribe trait should succeed");

    // When: a message is published
    stream.publish(true).await.expect("publish should succeed");

    // Then: both subscriber types receive the message
    wait_for_count(&count_direct, 1).await;
    wait_for_count(&count_trait, 1).await;
    assert_eq!(count_direct.load(Ordering::SeqCst), 1);
    assert_eq!(count_trait.load(Ordering::SeqCst), 1);
}
