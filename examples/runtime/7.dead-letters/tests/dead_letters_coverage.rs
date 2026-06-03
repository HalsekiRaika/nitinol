//! Additional integration tests for the 7.dead-letters example.
//!
//! Supplements `dead_letters.rs` with coverage for the `Query` inner message
//! type — the one DLQ scenario not yet covered in that file.

use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use nitinol_runtime::process::SubscriberContext;
use nitinol_runtime::{BoxedMessage, DeadLetter, ProcessSystem, Props, Subscriber};

use dead_letters::message::Query;
use dead_letters::target::TargetProcess;

// ─── helper ──────────────────────────────────────────────────────────────────

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

/// Records whether the inner message of the first `DeadLetter` downcasts to `Query`.
struct QueryTypeCheck {
    count: Arc<AtomicU32>,
    is_query: Arc<AtomicBool>,
}

impl Subscriber<BoxedMessage> for QueryTypeCheck {
    fn recv(
        &mut self,
        msg: BoxedMessage,
        _ctx: &mut SubscriberContext<'_, BoxedMessage>,
    ) -> impl Future<Output = ()> + Send {
        let count = self.count.clone();
        let is_query = self.is_query.clone();
        async move {
            let Some(dl) = msg.downcast_ref::<DeadLetter>() else {
                return;
            };
            count.fetch_add(1, Ordering::SeqCst);
            if dl.message.downcast_ref::<Query>().is_some() {
                is_query.store(true, Ordering::SeqCst);
            }
        }
    }
}

// ─── tests ───────────────────────────────────────────────────────────────────

/// The inner message of the `DeadLetter` downcasts back to `Query` when an ask
/// to a stopped process routes to the dead-letter stream.
///
/// This complements `dead_letter_message_downcasts_back_to_ping` in
/// `dead_letters.rs` and `suppress_dead_letter_log_does_not_prevent_stream_delivery`
/// (which tests `Hush`), ensuring all three message types from the demo are covered.
#[tokio::test]
async fn dead_letter_inner_message_downcasts_back_to_query() {
    // Given: a subscriber that checks whether the inner message is Query
    let system = ProcessSystem::new().await;
    let dl_stream = system.dead_letter_stream();

    let count = Arc::new(AtomicU32::new(0));
    let is_query = Arc::new(AtomicBool::new(false));
    let observer_proxy = system
        .spawn(Props::subscriber({
            let count = count.clone();
            let is_query = is_query.clone();
            move || QueryTypeCheck {
                count: count.clone(),
                is_query: is_query.clone(),
            }
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

    // When: Query is asked to the stopped process (ask also routes to the DLQ stream)
    let _ = proxy.ask(Query).await;

    // Then: the inner message downcasts successfully to Query
    wait_for_count(&count, 1).await;
    assert!(
        is_query.load(Ordering::SeqCst),
        "DeadLetter.message should downcast to Query when an ask to a stopped process is made"
    );
}
