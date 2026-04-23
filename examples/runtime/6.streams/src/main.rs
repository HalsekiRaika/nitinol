//! Pub/Sub stream example: fan-out temperature readings via Stream<BoxedMessage>.
//!
//! Run with:
//!   cargo run -p streams
//!
//! # Learning objectives
//!
//! 1. **1対多のイベント配信基盤**
//!    `Stream<BoxedMessage>` は publish された値を登録済みの全 subscriber に
//!    同時配信する。これは CQRS+ES 統合時のドメインイベント配信の原型となる。
//!
//! 2. **Clone が必要な理由**
//!    `publish<M: Message>` は `BoxedMessage::new(msg)` でメッセージを
//!    `Arc<dyn Any>` に包み、各 subscriber に `clone()` して届ける。
//!    clone は Arc の参照カウントを増やすだけで深いコピーは発生しないが、
//!    コンパイル時に `T: Clone` が要求されるため `TemperatureReading` は
//!    `#[derive(Clone)]` が必須である。
//!
//! 3. **Subscriber<T> vs Receive<T>**
//!    - `AlertSubscriber`: `Subscriber<BoxedMessage>` + `Props::subscriber`
//!      → 戻り値が `()` でシンプル。ライフサイクルフックなし。
//!    - `DisplaySubscriber`: `Process + Receive<BoxedMessage>` + `Props::new`
//!      → 戻り値が `Result<(), E>`、`on_start`/`on_stop` フックあり。
//!
//! 4. **Subscriber 停止時の自動クリーンアップ**
//!    Stream は subscribe 時に `ctx.watch(pid)` でプロセス死活を監視する。
//!    subscriber が停止すると Terminated 通知が Stream の `on_terminated`
//!    フックに届き、該当エントリが自動削除される。

use std::time::Duration;

use nitinol_runtime::ident::ProcessName;
use nitinol_runtime::{BoxedMessage, Props, ProcessSystem};

use streams::alert::AlertSubscriber;
use streams::display::DisplaySubscriber;
use streams::reading::TemperatureReading;
use streams::sensor::{Measure, TemperatureSensor};

#[tokio::main]
async fn main() {
    println!("=== Streams Example ===\n");

    demo_fanout_broadcast().await;
    demo_unsubscribe().await;
    demo_auto_cleanup().await;
}

// ─── Fan-out broadcast ────────────────────────────────────────────────────────

async fn demo_fanout_broadcast() {
    println!("--- Fan-out broadcast ---");
    let system = ProcessSystem::new().await;
    let stream = system
        .spawn_stream::<BoxedMessage>(ProcessName::new("temperature"))
        .await
        .expect("spawn_stream should succeed");

    // AlertSubscriber: Subscriber<BoxedMessage> trait — simplified API, no Result
    let alert_proxy = system
        .spawn(Props::subscriber(|| AlertSubscriber::new(35.0, Default::default())))
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

    println!("Publishing 25.0 °C (below threshold → no alert)");
    sensor.tell(Measure(25.0)).await.ok();
    tokio::time::sleep(Duration::from_millis(100)).await;

    println!("Publishing 38.5 °C (above threshold → alert fires)");
    sensor.tell(Measure(38.5)).await.ok();
    tokio::time::sleep(Duration::from_millis(100)).await;

    sensor.stop().await.ok();
    println!();
}

// ─── Unsubscribe ─────────────────────────────────────────────────────────────

async fn demo_unsubscribe() {
    println!("--- Unsubscribe ---");
    let system = ProcessSystem::new().await;
    let stream = system
        .spawn_stream::<BoxedMessage>(ProcessName::new("temperature-unsub"))
        .await
        .expect("spawn_stream should succeed");

    let alert_proxy = system
        .spawn(Props::subscriber(|| AlertSubscriber::new(20.0, Default::default())))
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
    let reading = TemperatureReading { sensor: "sensor-2".to_string(), celsius: 25.0 };
    stream.publish(reading).await.ok();
    tokio::time::sleep(Duration::from_millis(100)).await;
    println!("Both subscribers received the first message.");

    stream
        .unsubscribe(alert_pid)
        .await
        .expect("unsubscribe should succeed");
    println!("AlertSubscriber unsubscribed.");

    let reading2 = TemperatureReading { sensor: "sensor-2".to_string(), celsius: 40.0 };
    stream.publish(reading2).await.ok();
    tokio::time::sleep(Duration::from_millis(100)).await;
    println!("Only DisplaySubscriber received the second message.\n");
}

// ─── Auto-cleanup on subscriber stop ─────────────────────────────────────────

async fn demo_auto_cleanup() {
    println!("--- Auto-cleanup on subscriber stop ---");
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

    println!("Stopping DisplaySubscriber...");
    stop_handle.stop().await.ok();
    // Allow the Terminated notification to propagate to the Stream's on_terminated hook.
    tokio::time::sleep(Duration::from_millis(100)).await;

    println!("Publishing after subscriber stopped — stream auto-removed dead entry.");
    sensor.tell(Measure(30.0)).await.ok();
    tokio::time::sleep(Duration::from_millis(100)).await;

    sensor.stop().await.ok();
    println!();
}
