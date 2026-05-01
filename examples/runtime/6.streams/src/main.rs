//! Pub/Sub stream example: fan-out temperature readings via Stream<BoxedMessage>.
//!
//! Run with:
//!   cargo run -p streams
//!
//! To enable tokio-console (opt-in):
//!   set RUSTFLAGS="--cfg tokio_unstable"
//!   cargo run --features console -p streams
//!
//! RUST_LOG can be set to override the default log level (defaults to `info`).
//!
//! # Learning objectives
//!
//! 1. **One-to-many event distribution**
//!    `Stream<BoxedMessage>` broadcasts every published value to all registered
//!    subscribers simultaneously. This is the foundational pattern for domain
//!    event distribution in a CQRS+ES architecture.
//!
//! 2. **Why Clone is required**
//!    `publish<M: Message>` wraps the message with `BoxedMessage::new(msg)` into
//!    an `Arc<dyn Any>` and delivers a `clone()` to each subscriber.
//!    Cloning only increments the Arc reference count — no deep copy occurs —
//!    but the compiler requires `T: Clone` at compile time, so
//!    `TemperatureReading` must derive `#[derive(Clone)]`.
//!
//! 3. **Subscriber<T> vs Receive<T>**
//!    - `AlertSubscriber`: `Subscriber<BoxedMessage>` + `Props::subscriber`
//!      → returns `()`, simple API with no lifecycle hooks.
//!    - `DisplaySubscriber`: `Process + Receive<BoxedMessage>` + `Props::new`
//!      → returns `Result<(), E>`, with `on_start`/`on_stop` lifecycle hooks.
//!
//! 4. **Automatic cleanup on subscriber stop**
//!    The Stream monitors each subscriber's liveness via `ctx.watch(pid)` at
//!    subscribe time. When a subscriber stops, a Terminated notification is
//!    delivered to the Stream's `on_terminated` hook, which automatically
//!    removes the corresponding entry.

use std::time::Duration;

use nitinol_runtime::ident::ProcessName;
use nitinol_runtime::{BoxedMessage, ProcessSystem, Props};
use tracing::info;

use streams::alert::AlertSubscriber;
use streams::display::DisplaySubscriber;
use streams::reading::TemperatureReading;
use streams::sensor::{Measure, TemperatureSensor};

// noinspection DuplicatedCode
/// Initialise tracing for this example.
///
/// Without `--features console`:
///   Uses `fmt` + `EnvFilter`.  `RUST_LOG` controls the filter level;
///   defaults to `info` when unset.
///
/// With `--features console` (requires `RUSTFLAGS="--cfg tokio_unstable"`):
///   Adds a `console_subscriber` layer so the process can be inspected with
///   `tokio-console`.  Run with:
///     RUSTFLAGS="--cfg tokio_unstable" cargo run -p streams --features console
fn init_tracing() {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    use tracing_subscriber::{fmt, EnvFilter};

    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    #[cfg(not(feature = "console"))]
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(env_filter)
        .init();

    #[cfg(feature = "console")]
    tracing_subscriber::registry()
        .with(console_subscriber::spawn())
        .with(fmt::layer())
        .with(env_filter)
        .init();
}

#[tokio::main]
async fn main() {
    init_tracing();

    info!("Streams Example");

    demo_fanout_broadcast().await;
    demo_unsubscribe().await;
    demo_auto_cleanup().await;
}

// ─── Fan-out broadcast ────────────────────────────────────────────────────────
#[tracing::instrument(name = "demo-fanout-broadcast")]
async fn demo_fanout_broadcast() {
    info!("Fan-out broadcast");
    let system = ProcessSystem::new().await;
    let stream = system
        .spawn_stream::<BoxedMessage>(ProcessName::new("temperature"))
        .await
        .expect("spawn_stream should succeed");

    // AlertSubscriber: Subscriber<BoxedMessage> trait — simplified API, no Result
    let alert_proxy = system
        .spawn(Props::subscriber(|| {
            AlertSubscriber::new(35.0, Default::default())
        }))
        .await;
    stream
        .subscribe(alert_proxy)
        .await
        .expect("subscribe alert should succeed");

    // DisplaySubscriber: Process + Receive<BoxedMessage> — full lifecycle hooks
    let display_proxy = system
        .spawn(Props::new(|| DisplaySubscriber::new(Default::default())))
        .await;
    stream
        .subscribe(display_proxy)
        .await
        .expect("subscribe display should succeed");

    // TemperatureSensor holds a clone of the stream proxy (factory closure is Fn())
    let stream_for_sensor = stream.clone();
    let sensor = system
        .spawn(Props::new(move || {
            TemperatureSensor::new("sensor-1".to_string(), stream_for_sensor.clone())
        }))
        .await;

    info!("Publishing 25.0 °C (below threshold → no alert)");
    sensor.tell(Measure(25.0)).await.ok();
    tokio::time::sleep(Duration::from_millis(100)).await;

    info!("Publishing 38.5 °C (above threshold → alert fires)");
    sensor.tell(Measure(38.5)).await.ok();
    tokio::time::sleep(Duration::from_millis(100)).await;

    sensor.stop().await.ok();
}

// ─── Unsubscribe ─────────────────────────────────────────────────────────────
#[tracing::instrument(name = "demo-unsubscribe")]
async fn demo_unsubscribe() {
    info!("Unsubscribe");
    let system = ProcessSystem::new().await;
    let stream = system
        .spawn_stream::<BoxedMessage>(ProcessName::new("temperature-unsub"))
        .await
        .expect("spawn_stream should succeed");

    let alert_proxy = system
        .spawn(Props::subscriber(|| {
            AlertSubscriber::new(20.0, Default::default())
        }))
        .await;
    // pid must be captured before subscribe() moves the proxy
    let alert_pid = alert_proxy.pid();
    stream
        .subscribe(alert_proxy)
        .await
        .expect("subscribe alert should succeed");

    let display_proxy = system
        .spawn(Props::new(|| DisplaySubscriber::new(Default::default())))
        .await;
    stream
        .subscribe(display_proxy)
        .await
        .expect("subscribe display should succeed");

    // Publish a direct TemperatureReading to demonstrate bypass of sensor
    let reading = TemperatureReading {
        sensor: "sensor-2".to_string(),
        celsius: 25.0,
    };
    stream.publish(reading).await.ok();
    tokio::time::sleep(Duration::from_millis(100)).await;
    info!("Both subscribers received the first message.");

    stream
        .unsubscribe(alert_pid)
        .await
        .expect("unsubscribe should succeed");
    info!("AlertSubscriber unsubscribed.");

    let reading2 = TemperatureReading {
        sensor: "sensor-2".to_string(),
        celsius: 40.0,
    };
    stream.publish(reading2).await.ok();
    tokio::time::sleep(Duration::from_millis(100)).await;
    info!("Only DisplaySubscriber received the second message.");
}

// ─── Auto-cleanup on subscriber stop ─────────────────────────────────────────
#[tracing::instrument(name = "demo-auto-cleanup")]
async fn demo_auto_cleanup() {
    info!("Auto-cleanup on subscriber stop");
    let system = ProcessSystem::new().await;
    let stream = system
        .spawn_stream::<BoxedMessage>(ProcessName::new("temperature-cleanup"))
        .await
        .expect("spawn_stream should succeed");

    let display_proxy = system
        .spawn(Props::new(|| DisplaySubscriber::new(Default::default())))
        .await;
    let stop_handle = display_proxy.clone();
    stream
        .subscribe(display_proxy)
        .await
        .expect("subscribe should succeed");

    let stream_for_sensor = stream.clone();
    let sensor = system
        .spawn(Props::new(move || {
            TemperatureSensor::new("sensor-3".to_string(), stream_for_sensor.clone())
        }))
        .await;

    sensor.tell(Measure(22.0)).await.ok();
    tokio::time::sleep(Duration::from_millis(100)).await;

    info!("Stopping DisplaySubscriber...");
    stop_handle.stop().await.ok();
    // Allow the Terminated notification to propagate to the Stream's on_terminated hook.
    tokio::time::sleep(Duration::from_millis(100)).await;

    info!("Publishing after subscriber stopped — stream auto-removed dead entry.");
    sensor.tell(Measure(30.0)).await.ok();
    tokio::time::sleep(Duration::from_millis(100)).await;

    sensor.stop().await.ok();
}
