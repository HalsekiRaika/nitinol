mod common;

use std::time::Duration;

use nitinol_runtime::ident::ProcessName;
use nitinol_runtime::{ProcessSystem, Props, SupervisionStrategy};

use common::{test_props, tracked_state, TrackedProcess};

#[tokio::test]
async fn spawn_returns_proxy_with_unique_pid() {
    // Given: a ProcessSystem and two sets of Props
    let system = ProcessSystem::new().await;
    let (s1, st1, c1) = tracked_state();
    let (s2, st2, c2) = tracked_state();
    let props1 = test_props(s1, st1, c1);
    let props2 = test_props(s2, st2, c2);

    // When: two processes are spawned
    let proxy1 = system.spawn(props1).await;
    let proxy2 = system.spawn(props2).await;

    // Then: each proxy has a distinct Pid
    assert_ne!(proxy1.pid(), proxy2.pid());
}

#[tokio::test]
async fn spawn_named_returns_proxy() {
    // Given: a ProcessSystem and named Props
    let system = ProcessSystem::new().await;
    let (started, stopped, counter) = tracked_state();
    let props = test_props(started, stopped, counter);
    let name = ProcessName::new("test-process");

    // When: a named process is spawned
    let proxy = system.spawn(props.with_name(name)).await;

    // Then: the proxy has a valid Pid
    let _pid = proxy.pid();
}

#[tokio::test]
async fn props_new_creates_with_producer() {
    // Given: a producer closure
    let (started, stopped, counter) = tracked_state();

    // When: Props is created with the producer
    let props: Props<TrackedProcess> =
        Props::new(move || TrackedProcess::new(started.clone(), stopped.clone(), counter.clone()));

    // Then: Props is successfully created (compiles and doesn't panic)
    drop(props);
}

#[tokio::test]
async fn props_builder_sets_supervision_strategy() {
    // Given: Props with default strategy
    let (started, stopped, counter) = tracked_state();
    let props = test_props(started, stopped, counter);

    // When: supervision strategy is changed via builder
    let _props = props.with_supervision_strategy(
        SupervisionStrategy::restart(3, Duration::from_secs(60))
            .expect("restart with positive duration must succeed"),
    );

    // Then: no panic (strategy configured successfully)
}

#[tokio::test]
async fn supervision_strategy_stop_variant_exists() {
    // Given/When: Stop variant
    let strategy = SupervisionStrategy::Stop;

    // Then: matches Stop
    assert!(matches!(strategy, SupervisionStrategy::Stop));
}

#[tokio::test]
async fn supervision_strategy_restart_holds_configuration() {
    // Given/When: Restart variant with configuration
    let strategy = SupervisionStrategy::restart(5, Duration::from_secs(120))
        .expect("valid restart config");

    // Then: fields hold the provided values
    match strategy {
        SupervisionStrategy::Restart(config) => {
            assert_eq!(config.max_retries(), 5);
            assert_eq!(config.within(), Duration::from_secs(120));
        }
        _ => panic!("expected Restart variant"),
    }
}
