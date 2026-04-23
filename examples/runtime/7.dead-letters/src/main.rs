//! Dead Letter Queue example: capturing undeliverable messages via the built-in DLQ stream.
//!
//! Run with:
//!   cargo run -p dead-letters
//!
//! # Learning objectives
//!
//! 1. **組み込み Dead Letter インフラ**
//!    `ProcessSystem` は起動時に `$dead-letters` トピックの
//!    `Stream<BoxedMessage>` を自動で生成する。DLQ を有効にするために
//!    追加の設定は不要。
//!
//! 2. **Tell/Ask 失敗時の自動ルーティング**
//!    停止済みプロセスへの `tell` や `ask` が失敗すると、ランタイムは
//!    そのメッセージを `DeadLetter { destination, message, sender }` に
//!    包んで DLQ ストリームへ自動配信する。
//!
//! 3. **DeadLetter 構造体の検査**
//!    DLQ ストリームの subscriber は `BoxedMessage` を受け取り、
//!    `msg.downcast_ref::<DeadLetter>()` で外側を取り出した後、
//!    `dl.message.downcast_ref::<Ping>()` で内包メッセージを復元できる
//!    （二段 downcast）。
//!
//! 4. **SuppressDeadLetterLog マーカー**
//!    `SuppressDeadLetterLog` を実装した型のメッセージはランタイムが
//!    ログ出力だけを抑制する。ストリームへの配信は継続する。
//!    6.streams で学んだ Pub/Sub の仕組みが DLQ の基盤でもあることを
//!    ここで確認できる。

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use nitinol_runtime::{Props, ProcessSystem};

use dead_letters::message::{Hush, Ping, Query};
use dead_letters::observer::{DeadLetterInspector, DeadLetterObserver};
use dead_letters::target::TargetProcess;

/// Polls `counter` until it reaches at least `expected`, with a 5-second timeout.
async fn wait_for_count(counter: &AtomicU32, expected: u32) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while counter.load(Ordering::SeqCst) < expected {
        assert!(Instant::now() < deadline, "timed out waiting for dead letter");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::main]
async fn main() {
    println!("=== Dead Letters Example ===\n");

    demo_tell_routes_to_dead_letter_stream().await;
    demo_ask_returns_dead_letter_error_and_publishes_to_stream().await;
    demo_suppress_log_marker_still_delivers_to_stream().await;
}

// ─── Demo 1: Tell routes to dead-letter stream ───────────────────────────────

async fn demo_tell_routes_to_dead_letter_stream() {
    println!("--- Tell routes to dead-letter stream ---");

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

    // Inspector performs the real two-stage downcast and prints actual field values.
    let inspector_proxy = system.spawn(Props::subscriber(|| DeadLetterInspector)).await;
    dl_stream
        .subscribe(inspector_proxy)
        .await
        .expect("subscribe should succeed");

    // Spawn and immediately stop the target so all subsequent messages are undeliverable.
    let proxy = system.spawn(Props::new(|| TargetProcess)).await;
    let target_pid = proxy.pid();
    proxy.stop().await.expect("stop should succeed");
    tokio::time::sleep(Duration::from_millis(50)).await;

    println!("Sending Ping to stopped process (pid={target_pid})...");
    let _ = proxy.tell(Ping).await;

    // Poll until the observer confirms receipt, up to 5 seconds.
    wait_for_count(&received, 1).await;

    println!("DeadLetter received on stream (destination={target_pid}).\n");
}

// ─── Demo 2: Ask returns AskError::DeadLetter and publishes to stream ────────

async fn demo_ask_returns_dead_letter_error_and_publishes_to_stream() {
    println!("--- Ask returns AskError::DeadLetter and publishes to stream ---");

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

    // Inspector performs the real two-stage downcast and prints actual field values.
    let inspector_proxy = system.spawn(Props::subscriber(|| DeadLetterInspector)).await;
    dl_stream
        .subscribe(inspector_proxy)
        .await
        .expect("subscribe should succeed");

    let proxy = system.spawn(Props::new(|| TargetProcess)).await;
    let target_pid = proxy.pid();
    proxy.stop().await.expect("stop should succeed");
    tokio::time::sleep(Duration::from_millis(50)).await;

    println!("Asking Query to stopped process (pid={target_pid})...");
    let result = proxy.ask(Query).await;

    match result {
        Err(err) => println!("  ask() returned Err: {err}"),
        Ok(_) => println!("  ask() unexpectedly succeeded"),
    }

    // ask() also routes to the dead-letter stream.
    wait_for_count(&received, 1).await;

    println!("  stream also received the dead letter (count={}).\n", received.load(Ordering::SeqCst));
}

// ─── Demo 3: SuppressDeadLetterLog still delivers to stream ──────────────────

async fn demo_suppress_log_marker_still_delivers_to_stream() {
    println!("--- SuppressDeadLetterLog still delivers to stream ---");

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

    // Inspector performs the real two-stage downcast and prints actual field values.
    let inspector_proxy = system.spawn(Props::subscriber(|| DeadLetterInspector)).await;
    dl_stream
        .subscribe(inspector_proxy)
        .await
        .expect("subscribe should succeed");

    let proxy = system.spawn(Props::new(|| TargetProcess)).await;
    let target_pid = proxy.pid();
    proxy.stop().await.expect("stop should succeed");
    tokio::time::sleep(Duration::from_millis(50)).await;

    println!("Sending Hush (SuppressDeadLetterLog) to stopped process (pid={target_pid})...");
    let _ = proxy.tell(Hush).await;

    // Even though Hush suppresses log output, the stream still receives the notification.
    wait_for_count(&received, 1).await;

    println!("  Stream received the dead letter despite SuppressDeadLetterLog.");
    println!("  Log suppression is applied by the runtime — but stream delivery is never suppressed.\n");
}
