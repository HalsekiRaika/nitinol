#![cfg(feature = "test-helpers")]

use nitinol_eventsource::test_helpers::MockAggregateProxy;
use nitinol_eventsource::{Aggregate, Decider, Decision, Event};
use nitinol_persistence::{EventType, Family, TypeName};

#[derive(Clone, Debug, PartialEq)]
struct Reserved {
    sku: String,
}

impl Event for Reserved {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("mock.proxy"), TypeName::new("Reserved"));
}

#[derive(Default)]
struct Inventory;

impl Aggregate for Inventory {
    type Event = Reserved;

    fn apply(&mut self, _event: Reserved) {}
}

#[derive(Debug, PartialEq)]
struct Reserve {
    sku: String,
}

#[derive(Debug, PartialEq)]
struct Cancel {
    sku: String,
}

impl Decider<Reserve> for Inventory {
    type Output = ();
    type Rejection = std::convert::Infallible;

    fn decide(&self, _cmd: Reserve) -> Decision<Reserved, (), Self::Rejection> {
        Decision::persist(Vec::new()).output(())
    }
}

impl Decider<Cancel> for Inventory {
    type Output = ();
    type Rejection = std::convert::Infallible;

    fn decide(&self, _cmd: Cancel) -> Decision<Reserved, (), Self::Rejection> {
        Decision::persist(Vec::new()).output(())
    }
}

#[test]
fn new_yields_empty_capture_buffer() {
    let mock = MockAggregateProxy::<Inventory>::new();
    assert!(
        mock.drain_captured::<Reserve>().is_empty(),
        "a brand-new MockAggregateProxy must have no captured commands"
    );
}

#[test]
fn default_yields_empty_capture_buffer() {
    let mock: MockAggregateProxy<Inventory> = MockAggregateProxy::default();
    assert!(
        mock.drain_captured::<Reserve>().is_empty(),
        "Default::default() must agree with new() — both empty"
    );
}

#[tokio::test]
async fn tell_captures_the_dispatched_command() {
    let mock = MockAggregateProxy::<Inventory>::new();
    mock.tell(Reserve {
        sku: "SKU-1".into(),
    })
    .await
    .expect("MockAggregateProxy::tell must succeed for a valid command");
    let captured = mock.drain_captured::<Reserve>();
    assert_eq!(
        captured.len(),
        1,
        "exactly one Reserve command must be captured after a single tell"
    );
    assert_eq!(captured[0].sku, "SKU-1");
}

#[tokio::test]
async fn drain_captured_preserves_insertion_order() {
    let mock = MockAggregateProxy::<Inventory>::new();
    mock.tell(Reserve {
        sku: "first".into(),
    })
    .await
    .expect("tell must succeed");
    mock.tell(Reserve {
        sku: "second".into(),
    })
    .await
    .expect("tell must succeed");
    mock.tell(Reserve {
        sku: "third".into(),
    })
    .await
    .expect("tell must succeed");
    let captured = mock.drain_captured::<Reserve>();
    let skus: Vec<String> = captured.into_iter().map(|c| c.sku).collect();
    assert_eq!(
        skus,
        vec!["first", "second", "third"],
        "drain_captured must preserve the insertion order of tell() calls"
    );
}

#[tokio::test]
async fn drain_captured_returns_only_matching_type_and_leaves_others() {
    let mock = MockAggregateProxy::<Inventory>::new();
    mock.tell(Reserve { sku: "R-1".into() })
        .await
        .expect("tell Reserve must succeed");
    mock.tell(Cancel { sku: "C-1".into() })
        .await
        .expect("tell Cancel must succeed");
    mock.tell(Reserve { sku: "R-2".into() })
        .await
        .expect("tell Reserve must succeed");

    let reserves = mock.drain_captured::<Reserve>();
    let reserve_skus: Vec<String> = reserves.into_iter().map(|c| c.sku).collect();
    assert_eq!(
        reserve_skus,
        vec!["R-1", "R-2"],
        "drain_captured::<Reserve> must yield only Reserve commands, in order"
    );

    let cancels = mock.drain_captured::<Cancel>();
    let cancel_skus: Vec<String> = cancels.into_iter().map(|c| c.sku).collect();
    assert_eq!(
        cancel_skus,
        vec!["C-1"],
        "drain_captured::<Cancel> must yield the Cancel commands that remained"
    );
}

#[tokio::test]
async fn drain_captured_consumes_matching_commands_so_second_call_is_empty() {
    let mock = MockAggregateProxy::<Inventory>::new();
    mock.tell(Reserve { sku: "only".into() })
        .await
        .expect("tell must succeed");
    let first = mock.drain_captured::<Reserve>();
    let second = mock.drain_captured::<Reserve>();
    assert_eq!(
        first.len(),
        1,
        "the first drain must yield the captured command"
    );
    assert!(
        second.is_empty(),
        "drain_captured must consume the matching items so the second call sees nothing"
    );
}

#[tokio::test]
async fn clone_shares_capture_buffer_with_original() {
    let mock = MockAggregateProxy::<Inventory>::new();
    let mock_clone = mock.clone();
    mock_clone
        .tell(Reserve {
            sku: "shared".into(),
        })
        .await
        .expect("tell on clone must succeed");
    let captured = mock.drain_captured::<Reserve>();
    assert_eq!(
        captured.len(),
        1,
        "tells through a clone must be observable through the original"
    );
    assert_eq!(captured[0].sku, "shared");
}

#[tokio::test]
async fn original_and_clone_drain_from_the_same_buffer() {
    let mock = MockAggregateProxy::<Inventory>::new();
    let mock_clone = mock.clone();
    mock.tell(Reserve {
        sku: "via-original".into(),
    })
    .await
    .expect("tell must succeed");
    let captured = mock_clone.drain_captured::<Reserve>();
    assert_eq!(
        captured.len(),
        1,
        "tells through the original must be observable through a clone"
    );
    assert_eq!(captured[0].sku, "via-original");
    assert!(
        mock.drain_captured::<Reserve>().is_empty(),
        "drain through the clone must consume from the shared buffer"
    );
}

#[test]
fn mock_aggregate_proxy_is_send_sync_static() {
    fn assert_send<T: Send>() {}
    fn assert_sync<T: Sync>() {}
    fn assert_static<T: 'static>() {}

    assert_send::<MockAggregateProxy<Inventory>>();
    assert_sync::<MockAggregateProxy<Inventory>>();
    assert_static::<MockAggregateProxy<Inventory>>();
}
