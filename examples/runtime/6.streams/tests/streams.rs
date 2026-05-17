//! Integration tests for the 6.streams example.
//!
//! These tests verify:
//! - TemperatureSensor publishes TemperatureReading to a stream when it receives Measure
//! - publish delivers to all subscribers (fan-out)
//! - AlertSubscriber only increments alert_count when celsius > threshold
//! - AlertSubscriber does NOT alert when celsius <= threshold (boundary)
//! - DisplaySubscriber receives every published message via Process + Receive<BoxedMessage>
//! - unsubscribe stops delivery to the unsubscribed subscriber
//! - Stopped subscriber is auto-removed; subsequent publish succeeds
//! - Props::subscriber API works for AlertSubscriber (Subscriber<BoxedMessage> trait path)

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use nitinol_runtime::ident::ProcessName;
use nitinol_runtime::{BoxedMessage, ProcessSystem, Props};

use streams::alert::AlertSubscriber;
use streams::display::DisplaySubscriber;
use streams::reading::TemperatureReading;
use streams::sensor::{Measure, TemperatureSensor};

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

// ─── TemperatureReading ───────────────────────────────────────────────────────

/// TemperatureReading must derive Clone so the Stream can broadcast one value
/// to multiple subscribers without deep copying (Stream<T> requires T: Clone).
#[tokio::test]
async fn temperature_reading_implements_clone_preserving_all_fields() {
    // Given: a TemperatureReading with known values
    let original = TemperatureReading {
        sensor: "sensor-1".to_string(),
        celsius: 37.5,
    };

    // When: cloned
    let cloned = original.clone();

    // Then: all fields are preserved
    assert_eq!(cloned.sensor, "sensor-1");
    assert!((cloned.celsius - 37.5).abs() < f64::EPSILON);
}

/// TemperatureReading must satisfy Message (= 'static + Send + Sync) so it can
/// be published to a Stream<BoxedMessage> via publish<M: Message>.
#[tokio::test]
async fn temperature_reading_boxed_and_downcast_recovers_original_value() {
    // Given: a TemperatureReading wrapped in BoxedMessage
    let reading = TemperatureReading {
        sensor: "sensor-x".to_string(),
        celsius: 42.0,
    };
    let boxed = BoxedMessage::new(reading);

    // When: downcast to TemperatureReading
    let result = boxed.downcast_ref::<TemperatureReading>();

    // Then: the original sensor name and temperature are recovered
    assert!(result.is_some());
    let recovered = result.unwrap();
    assert_eq!(recovered.sensor, "sensor-x");
    assert!((recovered.celsius - 42.0).abs() < f64::EPSILON);
}

// ─── fan-out broadcast ────────────────────────────────────────────────────────

/// Verifies that a single publish from TemperatureSensor reaches both an
/// AlertSubscriber (Subscriber<BoxedMessage>) and a DisplaySubscriber
/// (Process + Receive<BoxedMessage>) — the core fan-out behavior.
#[tokio::test]
async fn sensor_publish_delivers_to_all_subscribers() {
    // Given: a stream with one AlertSubscriber and one DisplaySubscriber
    let system = ProcessSystem::new().await;
    let topic = ProcessName::new("test-fanout");
    let stream = system
        .spawn_stream::<BoxedMessage>(topic)
        .await
        .expect("spawn_stream should succeed");

    let alert_count = Arc::new(AtomicU32::new(0));
    let display_count = Arc::new(AtomicU32::new(0));

    // AlertSubscriber uses Props::subscriber (Subscriber<BoxedMessage> trait)
    let alert_proxy = system
        .spawn(Props::subscriber({
            let alert_count = alert_count.clone();
            // Threshold is below the published temperature so alert fires
            move || AlertSubscriber::new(20.0, alert_count.clone())
        }))
        .await;
    stream
        .subscribe(alert_proxy)
        .await
        .expect("subscribe alert should succeed");

    // DisplaySubscriber uses Props::new (Process + Receive<BoxedMessage> trait)
    let display_proxy = system
        .spawn(Props::new({
            let display_count = display_count.clone();
            move || DisplaySubscriber::new(display_count.clone())
        }))
        .await;
    stream
        .subscribe(display_proxy)
        .await
        .expect("subscribe display should succeed");

    // TemperatureSensor holds a clone of the stream proxy to publish readings
    let stream_for_sensor = stream.clone();
    let sensor_proxy = system
        .spawn(Props::new(move || {
            TemperatureSensor::new("sensor-a".to_string(), stream_for_sensor.clone())
        }))
        .await;

    // When: a temperature reading above threshold is measured
    sensor_proxy
        .tell(Measure(25.0))
        .await
        .expect("tell Measure should succeed");

    // Then: both subscribers receive exactly one message
    wait_for_count(&alert_count, 1).await;
    wait_for_count(&display_count, 1).await;
    assert_eq!(alert_count.load(Ordering::SeqCst), 1);
    assert_eq!(display_count.load(Ordering::SeqCst), 1);

    sensor_proxy.stop().await.ok();
}

// ─── AlertSubscriber threshold logic ─────────────────────────────────────────

/// Verifies that AlertSubscriber fires alert_count only for readings that
/// strictly exceed the threshold (celsius > threshold).
#[tokio::test]
async fn alert_fires_only_for_readings_above_threshold() {
    // Given: AlertSubscriber with threshold 35.0 and DisplaySubscriber for sync
    let system = ProcessSystem::new().await;
    let topic = ProcessName::new("test-alert-thresh");
    let stream = system
        .spawn_stream::<BoxedMessage>(topic)
        .await
        .expect("spawn_stream should succeed");

    let alert_count = Arc::new(AtomicU32::new(0));
    let display_count = Arc::new(AtomicU32::new(0));

    let alert_proxy = system
        .spawn(Props::subscriber({
            let alert_count = alert_count.clone();
            move || AlertSubscriber::new(35.0, alert_count.clone())
        }))
        .await;
    stream
        .subscribe(alert_proxy)
        .await
        .expect("subscribe alert should succeed");

    // DisplaySubscriber used as a synchronization point: when display_count
    // reaches N, all N messages have been processed by all subscribers.
    let display_proxy = system
        .spawn(Props::new({
            let display_count = display_count.clone();
            move || DisplaySubscriber::new(display_count.clone())
        }))
        .await;
    stream
        .subscribe(display_proxy)
        .await
        .expect("subscribe display should succeed");

    let stream_for_sensor = stream.clone();
    let sensor_proxy = system
        .spawn(Props::new(move || {
            TemperatureSensor::new("sensor-b".to_string(), stream_for_sensor.clone())
        }))
        .await;

    // When: a reading below threshold is published
    sensor_proxy
        .tell(Measure(25.0))
        .await
        .expect("tell should succeed");
    wait_for_count(&display_count, 1).await;

    // Then: no alert fires
    assert_eq!(
        alert_count.load(Ordering::SeqCst),
        0,
        "reading below threshold must not trigger alert"
    );

    // When: a reading above threshold is published
    sensor_proxy
        .tell(Measure(38.5))
        .await
        .expect("tell should succeed");
    wait_for_count(&display_count, 2).await;

    // Then: exactly one alert fires
    assert_eq!(
        alert_count.load(Ordering::SeqCst),
        1,
        "reading above threshold must trigger exactly one alert"
    );

    sensor_proxy.stop().await.ok();
}

/// Verifies the boundary condition: a reading exactly equal to the threshold
/// does NOT trigger an alert (only strictly greater values do).
#[tokio::test]
async fn alert_does_not_fire_when_celsius_equals_threshold() {
    // Given: AlertSubscriber with threshold 35.0
    let system = ProcessSystem::new().await;
    let topic = ProcessName::new("test-alert-boundary");
    let stream = system
        .spawn_stream::<BoxedMessage>(topic)
        .await
        .expect("spawn_stream should succeed");

    let alert_count = Arc::new(AtomicU32::new(0));
    let display_count = Arc::new(AtomicU32::new(0));

    let alert_proxy = system
        .spawn(Props::subscriber({
            let alert_count = alert_count.clone();
            move || AlertSubscriber::new(35.0, alert_count.clone())
        }))
        .await;
    stream
        .subscribe(alert_proxy)
        .await
        .expect("subscribe should succeed");

    let display_proxy = system
        .spawn(Props::new({
            let display_count = display_count.clone();
            move || DisplaySubscriber::new(display_count.clone())
        }))
        .await;
    stream
        .subscribe(display_proxy)
        .await
        .expect("subscribe display should succeed");

    let stream_for_sensor = stream.clone();
    let sensor_proxy = system
        .spawn(Props::new(move || {
            TemperatureSensor::new("sensor-c".to_string(), stream_for_sensor.clone())
        }))
        .await;

    // When: a reading exactly equal to threshold is published
    sensor_proxy
        .tell(Measure(35.0))
        .await
        .expect("tell should succeed");
    wait_for_count(&display_count, 1).await;

    // Then: alert does NOT fire (boundary: not strictly greater)
    assert_eq!(
        alert_count.load(Ordering::SeqCst),
        0,
        "reading equal to threshold must not trigger alert"
    );

    sensor_proxy.stop().await.ok();
}

/// Verifies that multiple readings result in the correct number of alerts:
/// only those strictly above the threshold are counted.
#[tokio::test]
async fn alert_count_tracks_only_above_threshold_readings() {
    // Given: AlertSubscriber with threshold 30.0
    let system = ProcessSystem::new().await;
    let topic = ProcessName::new("test-mixed-temps");
    let stream = system
        .spawn_stream::<BoxedMessage>(topic)
        .await
        .expect("spawn_stream should succeed");

    let alert_count = Arc::new(AtomicU32::new(0));
    let display_count = Arc::new(AtomicU32::new(0));

    let alert_proxy = system
        .spawn(Props::subscriber({
            let alert_count = alert_count.clone();
            move || AlertSubscriber::new(30.0, alert_count.clone())
        }))
        .await;
    stream
        .subscribe(alert_proxy)
        .await
        .expect("subscribe alert should succeed");

    let display_proxy = system
        .spawn(Props::new({
            let display_count = display_count.clone();
            move || DisplaySubscriber::new(display_count.clone())
        }))
        .await;
    stream
        .subscribe(display_proxy)
        .await
        .expect("subscribe display should succeed");

    let stream_for_sensor = stream.clone();
    let sensor_proxy = system
        .spawn(Props::new(move || {
            TemperatureSensor::new("sensor-h".to_string(), stream_for_sensor.clone())
        }))
        .await;

    // When: 5 readings are published (3 above threshold, 2 at/below)
    //   25.0 → below  (no alert)
    //   35.0 → above  (alert)
    //   30.0 → equal  (no alert, boundary)
    //   40.0 → above  (alert)
    //   32.0 → above  (alert)
    sensor_proxy
        .tell(Measure(25.0))
        .await
        .expect("tell should succeed");
    sensor_proxy
        .tell(Measure(35.0))
        .await
        .expect("tell should succeed");
    sensor_proxy
        .tell(Measure(30.0))
        .await
        .expect("tell should succeed");
    sensor_proxy
        .tell(Measure(40.0))
        .await
        .expect("tell should succeed");
    sensor_proxy
        .tell(Measure(32.0))
        .await
        .expect("tell should succeed");

    // Wait for all 5 messages to be processed by display subscriber
    wait_for_count(&display_count, 5).await;

    // Then: exactly 3 alerts triggered
    assert_eq!(
        alert_count.load(Ordering::SeqCst),
        3,
        "only readings strictly above threshold should trigger alerts"
    );

    sensor_proxy.stop().await.ok();
}

// ─── DisplaySubscriber ────────────────────────────────────────────────────────

/// Verifies that DisplaySubscriber (Process + Receive<BoxedMessage>) increments
/// its counter for every received message, regardless of temperature value.
#[tokio::test]
async fn display_subscriber_counts_every_received_message() {
    // Given: a DisplaySubscriber subscribed to a stream
    let system = ProcessSystem::new().await;
    let topic = ProcessName::new("test-display");
    let stream = system
        .spawn_stream::<BoxedMessage>(topic)
        .await
        .expect("spawn_stream should succeed");

    let display_count = Arc::new(AtomicU32::new(0));
    let display_proxy = system
        .spawn(Props::new({
            let display_count = display_count.clone();
            move || DisplaySubscriber::new(display_count.clone())
        }))
        .await;
    stream
        .subscribe(display_proxy)
        .await
        .expect("subscribe should succeed");

    let stream_for_sensor = stream.clone();
    let sensor_proxy = system
        .spawn(Props::new(move || {
            TemperatureSensor::new("sensor-d".to_string(), stream_for_sensor.clone())
        }))
        .await;

    // When: three temperature readings are published (mixed below/above threshold)
    sensor_proxy
        .tell(Measure(20.0))
        .await
        .expect("tell should succeed");
    sensor_proxy
        .tell(Measure(30.0))
        .await
        .expect("tell should succeed");
    sensor_proxy
        .tell(Measure(40.0))
        .await
        .expect("tell should succeed");

    // Then: display subscriber receives all three
    wait_for_count(&display_count, 3).await;
    assert_eq!(display_count.load(Ordering::SeqCst), 3);

    sensor_proxy.stop().await.ok();
}

// ─── AlertSubscriber via Props::subscriber ────────────────────────────────────

/// Verifies that AlertSubscriber can be spawned via Props::subscriber (the
/// simplified Subscriber<BoxedMessage> trait API) and receives published messages.
///
/// This test also verifies that publish can be called directly on the stream
/// proxy with a TemperatureReading value (bypassing the sensor), confirming
/// that TemperatureReading satisfies the Message bound.
#[tokio::test]
async fn alert_subscriber_via_props_subscriber_receives_above_threshold_reading() {
    // Given: AlertSubscriber spawned via Props::subscriber (no Props::new needed)
    let system = ProcessSystem::new().await;
    let topic = ProcessName::new("test-props-sub");
    let stream = system
        .spawn_stream::<BoxedMessage>(topic)
        .await
        .expect("spawn_stream should succeed");

    let alert_count = Arc::new(AtomicU32::new(0));
    let props = Props::subscriber({
        let alert_count = alert_count.clone();
        move || AlertSubscriber::new(35.0, alert_count.clone())
    });
    let proxy = system.spawn(props).await;
    stream
        .subscribe(proxy)
        .await
        .expect("subscribe should succeed");

    // When: a TemperatureReading above threshold is published directly to the stream
    let reading = TemperatureReading {
        sensor: "sensor-g".to_string(),
        celsius: 40.0,
    };
    stream
        .publish_boxed(reading)
        .await
        .expect("publish should succeed");

    // Then: the Subscriber<BoxedMessage> trait path fires the alert
    wait_for_count(&alert_count, 1).await;
    assert_eq!(alert_count.load(Ordering::SeqCst), 1);
}

// ─── unsubscribe ──────────────────────────────────────────────────────────────

/// Verifies that after unsubscribe, the removed subscriber no longer receives
/// messages while the remaining subscriber continues to receive.
#[tokio::test]
async fn unsubscribed_subscriber_does_not_receive_subsequent_messages() {
    // Given: a stream with two subscribers (alert + display)
    let system = ProcessSystem::new().await;
    let topic = ProcessName::new("test-unsub");
    let stream = system
        .spawn_stream::<BoxedMessage>(topic)
        .await
        .expect("spawn_stream should succeed");

    let alert_count = Arc::new(AtomicU32::new(0));
    let display_count = Arc::new(AtomicU32::new(0));

    // AlertSubscriber — will be unsubscribed mid-test
    let alert_proxy = system
        .spawn(Props::subscriber({
            let alert_count = alert_count.clone();
            move || AlertSubscriber::new(20.0, alert_count.clone())
        }))
        .await;
    // pid must be captured before subscribe() moves the proxy
    let alert_pid = alert_proxy.pid();
    stream
        .subscribe(alert_proxy)
        .await
        .expect("subscribe alert should succeed");

    // DisplaySubscriber — will stay subscribed throughout
    let display_proxy = system
        .spawn(Props::new({
            let display_count = display_count.clone();
            move || DisplaySubscriber::new(display_count.clone())
        }))
        .await;
    stream
        .subscribe(display_proxy)
        .await
        .expect("subscribe display should succeed");

    let stream_for_sensor = stream.clone();
    let sensor_proxy = system
        .spawn(Props::new(move || {
            TemperatureSensor::new("sensor-e".to_string(), stream_for_sensor.clone())
        }))
        .await;

    // Verify both receive the first message (above threshold → alert fires)
    sensor_proxy
        .tell(Measure(25.0))
        .await
        .expect("tell should succeed");
    wait_for_count(&alert_count, 1).await;
    wait_for_count(&display_count, 1).await;

    // When: alert subscriber is explicitly unsubscribed
    stream
        .unsubscribe(alert_pid)
        .await
        .expect("unsubscribe should succeed");

    // And: a second message is published (above threshold)
    sensor_proxy
        .tell(Measure(25.0))
        .await
        .expect("tell should succeed");
    wait_for_count(&display_count, 2).await;

    // Then: alert subscriber did NOT receive the second message
    assert_eq!(
        alert_count.load(Ordering::SeqCst),
        1,
        "unsubscribed subscriber must not receive subsequent messages"
    );
    // And: display subscriber still received the second message
    assert_eq!(
        display_count.load(Ordering::SeqCst),
        2,
        "remaining subscriber must still receive messages after partial unsubscribe"
    );

    sensor_proxy.stop().await.ok();
}

// ─── auto-cleanup on subscriber stop ─────────────────────────────────────────

/// Verifies that when a subscriber process is stopped, the Stream auto-removes
/// it via the Terminated notification (on_terminated hook), so subsequent
/// publishes neither error nor deliver to the dead subscriber.
#[tokio::test]
async fn stopped_subscriber_is_auto_removed_subsequent_publish_succeeds() {
    // Given: a stream with one display subscriber
    let system = ProcessSystem::new().await;
    let topic = ProcessName::new("test-auto-cleanup");
    let stream = system
        .spawn_stream::<BoxedMessage>(topic)
        .await
        .expect("spawn_stream should succeed");

    let display_count = Arc::new(AtomicU32::new(0));
    let display_proxy = system
        .spawn(Props::new({
            let display_count = display_count.clone();
            move || DisplaySubscriber::new(display_count.clone())
        }))
        .await;
    // Keep a handle to stop the subscriber later
    let stop_handle = display_proxy.clone();
    stream
        .subscribe(display_proxy)
        .await
        .expect("subscribe should succeed");

    let stream_for_sensor = stream.clone();
    let sensor_proxy = system
        .spawn(Props::new(move || {
            TemperatureSensor::new("sensor-f".to_string(), stream_for_sensor.clone())
        }))
        .await;

    // Verify the subscriber is active before stopping
    sensor_proxy
        .tell(Measure(25.0))
        .await
        .expect("tell should succeed");
    wait_for_count(&display_count, 1).await;

    // When: the subscriber is stopped
    stop_handle.stop().await.expect("stop should succeed");
    // Allow the Terminated notification to propagate from the registry to the
    // Stream's on_terminated hook, which removes the dead subscriber entry.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // And: a message is published after the subscriber is gone
    let result = sensor_proxy.tell(Measure(30.0)).await;

    // Then: publish succeeds — stream auto-removed the dead subscriber
    assert!(result.is_ok(), "publish after subscriber stop must succeed");

    // Allow any spurious delivery (should not happen) to settle
    tokio::time::sleep(Duration::from_millis(50)).await;

    // And: the stopped subscriber count remains at 1 (did not receive post-stop message)
    assert_eq!(
        display_count.load(Ordering::SeqCst),
        1,
        "stopped subscriber must not receive messages after stop"
    );

    sensor_proxy.stop().await.ok();
}

/// Verifies that when one of two subscribers is stopped, only that subscriber
/// is auto-removed; the remaining subscriber continues to receive messages.
#[tokio::test]
async fn only_stopped_subscriber_is_removed_remaining_stays_active() {
    // Given: a stream with two subscribers
    let system = ProcessSystem::new().await;
    let topic = ProcessName::new("test-partial-cleanup");
    let stream = system
        .spawn_stream::<BoxedMessage>(topic)
        .await
        .expect("spawn_stream should succeed");

    let alert_count = Arc::new(AtomicU32::new(0));
    let display_count = Arc::new(AtomicU32::new(0));

    let alert_proxy = system
        .spawn(Props::subscriber({
            let alert_count = alert_count.clone();
            move || AlertSubscriber::new(20.0, alert_count.clone())
        }))
        .await;
    let alert_stop_handle = alert_proxy.clone();
    stream
        .subscribe(alert_proxy)
        .await
        .expect("subscribe alert should succeed");

    let display_proxy = system
        .spawn(Props::new({
            let display_count = display_count.clone();
            move || DisplaySubscriber::new(display_count.clone())
        }))
        .await;
    stream
        .subscribe(display_proxy)
        .await
        .expect("subscribe display should succeed");

    let stream_for_sensor = stream.clone();
    let sensor_proxy = system
        .spawn(Props::new(move || {
            TemperatureSensor::new("sensor-i".to_string(), stream_for_sensor.clone())
        }))
        .await;

    // Verify both receive the first message
    sensor_proxy
        .tell(Measure(25.0))
        .await
        .expect("tell should succeed");
    wait_for_count(&alert_count, 1).await;
    wait_for_count(&display_count, 1).await;

    // When: alert subscriber is stopped
    alert_stop_handle.stop().await.expect("stop should succeed");
    tokio::time::sleep(Duration::from_millis(100)).await;

    // And: a second message is published
    sensor_proxy
        .tell(Measure(25.0))
        .await
        .expect("tell should succeed");
    wait_for_count(&display_count, 2).await;

    // Then: display subscriber receives the second message
    assert_eq!(
        display_count.load(Ordering::SeqCst),
        2,
        "remaining subscriber must still receive after partial cleanup"
    );
    // And: stopped subscriber did not receive the second message
    assert_eq!(
        alert_count.load(Ordering::SeqCst),
        1,
        "stopped subscriber must not receive messages after auto-removal"
    );

    sensor_proxy.stop().await.ok();
}
