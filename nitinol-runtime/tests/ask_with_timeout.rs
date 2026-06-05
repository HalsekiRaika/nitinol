mod common;

use std::future::Future;
use std::time::Duration;

use nitinol_runtime::error::AskError;
use nitinol_runtime::process::{Process, ProcessContext, Receive};
use nitinol_runtime::{ProcessSystem, Props};

use common::{test_props, tracked_state, wait_for_flag, GetCount};

struct SlowProcess;

impl Process for SlowProcess {}

struct SlowMessage {
    duration: Duration,
    value: u32,
}

impl Receive<SlowMessage> for SlowProcess {
    type Response = u32;
    type Error = std::convert::Infallible;
    fn recv(
        &mut self,
        msg: SlowMessage,
        _ctx: &mut ProcessContext<Self>,
    ) -> impl Future<Output = Result<u32, std::convert::Infallible>> + Send {
        async move {
            tokio::time::sleep(msg.duration).await;
            Ok(msg.value)
        }
    }
}

struct FastQuery;

impl Receive<FastQuery> for SlowProcess {
    type Response = u32;
    type Error = std::convert::Infallible;
    async fn recv(
        &mut self,
        _: FastQuery,
        _: &mut ProcessContext<Self>,
    ) -> Result<u32, std::convert::Infallible> {
        Ok(7)
    }
}

struct FailQuery;

#[derive(Debug, thiserror::Error)]
#[error("deliberate handler failure")]
struct FailQueryError;

impl Receive<FailQuery> for SlowProcess {
    type Response = u32;
    type Error = FailQueryError;
    async fn recv(
        &mut self,
        _: FailQuery,
        _: &mut ProcessContext<Self>,
    ) -> Result<u32, FailQueryError> {
        Err(FailQueryError)
    }
}

#[tokio::test]
async fn ask_error_timeout_implements_std_error() {
    use nitinol_runtime::ident::Pid;

    let system = ProcessSystem::new().await;
    let proxy = system.spawn(Props::new(|| SlowProcess)).await;
    let pid: Pid = proxy.pid();

    let err: AskError<std::convert::Infallible> = AskError::Timeout { destination: pid };

    let dyn_err: &dyn std::error::Error = &err;
    let rendered = dyn_err.to_string();

    assert!(
        !rendered.is_empty(),
        "AskError::Timeout Display must produce a non-empty message"
    );
}

#[tokio::test]
async fn ask_error_timeout_destination_is_pattern_matchable() {
    let system = ProcessSystem::new().await;
    let proxy = system.spawn(Props::new(|| SlowProcess)).await;
    let pid = proxy.pid();

    let err: AskError<std::convert::Infallible> = AskError::Timeout { destination: pid };

    match err {
        AskError::Timeout { destination } => assert_eq!(destination, pid),
        other => panic!("expected AskError::Timeout, got {:?}", other),
    }
}

#[tokio::test]
async fn ask_with_timeout_returns_ok_when_handler_replies_within_deadline() {
    let system = ProcessSystem::new().await;
    let proxy = system.spawn(Props::new(|| SlowProcess)).await;

    let result = proxy
        .ask_with_timeout(FastQuery, Duration::from_secs(5))
        .await;

    assert_eq!(result.expect("ask_with_timeout should succeed"), 7);
}

#[tokio::test]
async fn ask_with_timeout_does_not_fire_when_reply_arrives_just_before_deadline() {
    let system = ProcessSystem::new().await;
    let proxy = system.spawn(Props::new(|| SlowProcess)).await;

    let result = proxy
        .ask_with_timeout(
            SlowMessage {
                duration: Duration::from_millis(30),
                value: 99,
            },
            Duration::from_millis(500),
        )
        .await;

    assert_eq!(result.expect("ask_with_timeout should succeed"), 99);
}

#[tokio::test]
async fn ask_with_timeout_returns_timeout_when_handler_blocks_longer_than_deadline() {
    let system = ProcessSystem::new().await;
    let proxy = system.spawn(Props::new(|| SlowProcess)).await;

    let result = proxy
        .ask_with_timeout(
            SlowMessage {
                duration: Duration::from_millis(500),
                value: 1,
            },
            Duration::from_millis(50),
        )
        .await;

    match result {
        Err(AskError::Timeout { .. }) => {}
        other => panic!("expected Err(AskError::Timeout), got {:?}", other),
    }
}

#[tokio::test]
async fn ask_with_timeout_timeout_destination_is_target_pid() {
    let system = ProcessSystem::new().await;
    let proxy = system.spawn(Props::new(|| SlowProcess)).await;
    let expected_pid = proxy.pid();

    let result = proxy
        .ask_with_timeout(
            SlowMessage {
                duration: Duration::from_millis(500),
                value: 0,
            },
            Duration::from_millis(40),
        )
        .await;

    match result {
        Err(AskError::Timeout { destination }) => {
            assert_eq!(
                destination, expected_pid,
                "destination must be the target Pid, not the internal temp proxy Pid"
            );
        }
        other => panic!(
            "expected Err(AskError::Timeout {{ destination: {:?} }}), got {:?}",
            expected_pid, other
        ),
    }
}

#[tokio::test]
async fn ask_with_timeout_returns_handler_error_when_handler_fails() {
    let system = ProcessSystem::new().await;
    let proxy = system.spawn(Props::new(|| SlowProcess)).await;

    let result = proxy
        .ask_with_timeout(FailQuery, Duration::from_secs(5))
        .await;

    match result {
        Err(AskError::Handler(e)) => {
            assert_eq!(e.to_string(), "deliberate handler failure");
        }
        other => panic!("expected Err(AskError::Handler), got {:?}", other),
    }
}

#[tokio::test]
async fn ask_with_timeout_to_stopped_process_returns_dead_letter() {
    let system = ProcessSystem::new().await;
    let (started, stopped, counter) = tracked_state();
    let proxy = system
        .spawn(test_props(started.clone(), stopped.clone(), counter))
        .await;
    wait_for_flag(&started).await;
    proxy.stop().await.expect("stop should succeed");
    wait_for_flag(&stopped).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let result = proxy
        .ask_with_timeout(GetCount, Duration::from_secs(5))
        .await;

    match result {
        Err(AskError::DeadLetter { .. }) => {}
        other => panic!("expected Err(AskError::DeadLetter), got {:?}", other),
    }
}

#[tokio::test]
async fn ask_with_timeout_dead_letter_destination_matches_target_pid() {
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

    let result = proxy
        .ask_with_timeout(GetCount, Duration::from_secs(5))
        .await;

    match result.expect_err("ask_with_timeout to stopped process must fail") {
        AskError::DeadLetter { destination } => assert_eq!(destination, expected_pid),
        other => panic!(
            "expected AskError::DeadLetter {{ destination: {:?} }}, got {:?}",
            expected_pid, other
        ),
    }
}

#[tokio::test]
async fn ask_and_ask_with_timeout_coexist_on_same_proxy() {
    let system = ProcessSystem::new().await;
    let proxy = system.spawn(Props::new(|| SlowProcess)).await;

    let ask_result = proxy.ask(FastQuery).await;
    let ask_timeout_result = proxy
        .ask_with_timeout(FastQuery, Duration::from_secs(5))
        .await;

    assert_eq!(ask_result.expect("ask should succeed"), 7);
    assert_eq!(
        ask_timeout_result.expect("ask_with_timeout should succeed"),
        7
    );
}

#[tokio::test]
async fn target_process_survives_ask_with_timeout_timeout() {
    let system = ProcessSystem::new().await;
    let proxy = system.spawn(Props::new(|| SlowProcess)).await;

    let first = proxy
        .ask_with_timeout(
            SlowMessage {
                duration: Duration::from_millis(200),
                value: 0,
            },
            Duration::from_millis(50),
        )
        .await;
    assert!(
        matches!(first, Err(AskError::Timeout { .. })),
        "first ask_with_timeout must return AskError::Timeout, got {:?}",
        first
    );

    // FIFO mailbox: wait for the slow handler to finish so the queue drains
    // before the second ask.
    tokio::time::sleep(Duration::from_millis(250)).await;

    let second = proxy.ask(FastQuery).await;
    assert_eq!(second.expect("subsequent ask must succeed"), 7);
}

#[tokio::test]
async fn ask_with_timeout_recovers_after_prior_timeout() {
    let system = ProcessSystem::new().await;
    let proxy = system.spawn(Props::new(|| SlowProcess)).await;
    let first = proxy
        .ask_with_timeout(
            SlowMessage {
                duration: Duration::from_millis(200),
                value: 0,
            },
            Duration::from_millis(40),
        )
        .await;
    assert!(matches!(first, Err(AskError::Timeout { .. })));

    // Wait for the slow handler to drain so the mailbox is clear.
    tokio::time::sleep(Duration::from_millis(250)).await;

    let second = proxy
        .ask_with_timeout(FastQuery, Duration::from_secs(5))
        .await;

    assert_eq!(
        second.expect("second ask_with_timeout must succeed"),
        7,
        "ask_with_timeout must remain usable after a prior Timeout"
    );
}
