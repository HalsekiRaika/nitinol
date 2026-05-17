use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::Notify;

use nitinol_eventsource::Effect;
use nitinol_runtime::ident::ProcessName;
use nitinol_runtime::process::{Process, ProcessContext, ProcessProxy, Receive, Stream};
use nitinol_runtime::{ProcessSystem, Props};

// ---------------------------------------------------------------------------
// Fixtures: typed message
// ---------------------------------------------------------------------------

/// A typed domain message used to test typed stream publish.
///
/// Message is blanket-implemented for all T: 'static + Send + Sync, so no
/// explicit impl is needed.  Clone is required by Stream<T> broadcast semantics.
#[derive(Clone, Debug, PartialEq)]
struct TypedMsg {
    value: u32,
}

// ---------------------------------------------------------------------------
// Fixtures: receiver process
// ---------------------------------------------------------------------------

/// Captures each received TypedMsg so the test can inspect it.
///
/// Note: Receive<M> uses RPITIT (`-> impl Future + Send`), not #[async_trait].
/// Use plain `async fn recv` in the impl.
struct MsgReceiver {
    received: Arc<Mutex<Option<TypedMsg>>>,
    notify: Arc<Notify>,
}

impl Process for MsgReceiver {}

impl Receive<TypedMsg> for MsgReceiver {
    type Response = ();
    type Error = std::convert::Infallible;

    async fn recv(
        &mut self,
        msg: TypedMsg,
        _ctx: &mut ProcessContext,
    ) -> Result<(), Self::Error> {
        *self.received.lock().unwrap() = Some(msg);
        self.notify.notify_one();
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Test 1: Effect::publish with typed stream returns Side variant
// ---------------------------------------------------------------------------

/// Effect::publish(ProcessProxy<Stream<TypedMsg>>, TypedMsg) must return Effect::Side.
#[tokio::test]
async fn effect_publish_typed_returns_side_variant() {
    // Given: a running system with a typed Stream<TypedMsg>
    let system = ProcessSystem::new().await;
    let stream: ProcessProxy<Stream<TypedMsg>> = system
        .spawn_stream::<TypedMsg>(ProcessName::new("effect-publish-typed-side"))
        .await
        .expect("spawn_stream must succeed");

    // When: building a typed publish effect
    let effect = Effect::<()>::publish(stream, TypedMsg { value: 99 });

    // Then: the result is the Side variant
    assert!(
        matches!(effect, Effect::Side(_)),
        "Effect::publish(typed_stream, typed_msg) must return Effect::Side"
    );
}

// ---------------------------------------------------------------------------
// Test 2: Typed published message reaches typed subscriber — round-trip
// ---------------------------------------------------------------------------

/// A typed message published via Effect::publish must be delivered to the
/// subscriber of the typed stream.
///
/// This verifies the complete delivery path:
///   Effect::publish(stream, msg)
///     → Effect::Side(TypedPublish { stream, message: msg })
///     → TypedPublish::execute()
///     → stream.publish(msg)      ← new typed publish method on ProcessProxy<Stream<T>>
///     → subscriber.recv(msg)
#[tokio::test]
async fn effect_publish_typed_delivers_message_to_subscriber() {
    // Given: a typed stream with one subscriber
    let system = ProcessSystem::new().await;
    let stream: ProcessProxy<Stream<TypedMsg>> = system
        .spawn_stream::<TypedMsg>(ProcessName::new("effect-publish-typed-roundtrip"))
        .await
        .expect("spawn_stream must succeed");

    let received = Arc::new(Mutex::new(None::<TypedMsg>));
    let notify = Arc::new(Notify::new());
    let received_c = Arc::clone(&received);
    let notify_c = Arc::clone(&notify);

    let receiver_proxy = system
        .spawn(Props::new(move || MsgReceiver {
            received: Arc::clone(&received_c),
            notify: Arc::clone(&notify_c),
        }))
        .await;

    stream
        .subscribe(receiver_proxy)
        .await
        .expect("subscribe must succeed");

    // When: build and execute a typed publish effect
    let expected = TypedMsg { value: 42 };
    let effect = Effect::<()>::publish(stream, expected.clone());

    if let Effect::Side(side) = effect {
        side.execute()
            .await
            .expect("side effect execution must succeed");
    } else {
        panic!("expected Effect::Side");
    }

    // Then: the subscriber receives the exact message
    tokio::time::timeout(Duration::from_millis(500), async {
        loop {
            let notified = notify.notified();
            if received.lock().unwrap().is_some() {
                return;
            }
            notified.await;
        }
    })
    .await
    .expect("subscriber must receive message within 500 ms");

    let received_msg = received.lock().unwrap().clone().expect("must have received a message");
    assert_eq!(
        received_msg, expected,
        "subscriber must receive the exact TypedMsg that was published"
    );
}

// ---------------------------------------------------------------------------
// Test 3: Effect::publish is symmetric with Effect::tell (compile-time check)
// ---------------------------------------------------------------------------

/// Effect::publish<T>(stream: ProcessProxy<Stream<T>>, msg: T) must accept
/// the same type parameter T in both the stream and message position.
///
/// This is a compile-time check: if T is inferred from the stream (Stream<TypedMsg>),
/// then msg must also be TypedMsg.  Passing a different type would be a compile error.
///
/// The test itself is trivial at runtime; the valuable assertion is that it compiles
/// with a typed stream rather than Stream<BoxedMessage>.
#[tokio::test]
async fn effect_publish_type_parameter_is_consistent_between_stream_and_message() {
    // Given
    let system = ProcessSystem::new().await;
    let stream: ProcessProxy<Stream<TypedMsg>> = system
        .spawn_stream::<TypedMsg>(ProcessName::new("effect-publish-typed-consistency"))
        .await
        .expect("spawn_stream must succeed");

    let msg = TypedMsg { value: 7 };

    // When: publish with the same type T inferred from the stream
    // This must compile: stream is Stream<TypedMsg>, msg is TypedMsg
    let effect = Effect::<()>::publish(stream, msg);

    // Then: compilation succeeded and Side variant is produced
    assert!(
        matches!(effect, Effect::Side(_)),
        "typed publish must produce Effect::Side"
    );
}

// ---------------------------------------------------------------------------
// Test 4: Multiple subscribers all receive the typed message
// ---------------------------------------------------------------------------

/// When a Stream<TypedMsg> has multiple subscribers, all of them must receive
/// the message published via Effect::publish.  This tests the broadcast semantics
/// of the typed stream.
#[tokio::test]
async fn effect_publish_typed_broadcasts_to_all_subscribers() {
    // Given: a typed stream with two subscribers
    let system = ProcessSystem::new().await;
    let stream: ProcessProxy<Stream<TypedMsg>> = system
        .spawn_stream::<TypedMsg>(ProcessName::new("effect-publish-typed-broadcast"))
        .await
        .expect("spawn_stream must succeed");

    let make_receiver = |label: &'static str| {
        let received = Arc::new(Mutex::new(None::<TypedMsg>));
        let notify = Arc::new(Notify::new());
        let received_c = Arc::clone(&received);
        let notify_c = Arc::clone(&notify);
        let props = Props::new(move || MsgReceiver {
            received: Arc::clone(&received_c),
            notify: Arc::clone(&notify_c),
        });
        let _ = label; // suppress warning
        (received, notify, props)
    };

    let (recv_a, notify_a, props_a) = make_receiver("a");
    let (recv_b, notify_b, props_b) = make_receiver("b");

    let proxy_a = system.spawn(props_a).await;
    let proxy_b = system.spawn(props_b).await;

    stream.subscribe(proxy_a).await.expect("subscribe a");
    stream.subscribe(proxy_b).await.expect("subscribe b");

    // When: publish one message
    let expected = TypedMsg { value: 55 };
    let effect = Effect::<()>::publish(stream, expected.clone());

    if let Effect::Side(side) = effect {
        side.execute().await.expect("execute must succeed");
    } else {
        panic!("expected Effect::Side");
    }

    // Then: both subscribers receive the message
    for (recv, notify) in [(&recv_a, &notify_a), (&recv_b, &notify_b)] {
        tokio::time::timeout(Duration::from_millis(500), async {
            loop {
                let notified = notify.notified();
                if recv.lock().unwrap().is_some() {
                    return;
                }
                notified.await;
            }
        })
        .await
        .expect("each subscriber must receive the message within 500 ms");

        let msg = recv.lock().unwrap().clone().expect("must have received");
        assert_eq!(msg, expected, "all subscribers must receive the same typed message");
    }
}
