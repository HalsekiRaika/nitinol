mod common;
use common::{Shape, TestMsg, TestProcess, shape_of};
use nitinol_eventsource::Effect;
use nitinol_runtime::ident::ProcessName;
use nitinol_runtime::process::Props;
use nitinol_runtime::{BoxedMessage, ProcessSystem};

// ---------------------------------------------------------------------------
// Construction: empty()
// ---------------------------------------------------------------------------

/// empty() returns the None variant
#[test]
fn empty_returns_none_variant() {
    // Given / When
    let effect = Effect::<()>::empty();

    // Then
    assert!(
        matches!(effect, Effect::None),
        "empty() must return Effect::None"
    );
}

// ---------------------------------------------------------------------------
// Construction: persist()
// ---------------------------------------------------------------------------

/// persist(e) wraps a single event in the Persist variant with a one-element vec
#[test]
fn persist_wraps_single_event_in_persist_variant() {
    // Given
    let event = 42u32;

    // When
    let effect = Effect::persist(event);

    // Then
    assert_eq!(
        shape_of(&effect),
        Shape::Persist(vec![42u32]),
        "persist(e) must return Persist([e])"
    );
}

// ---------------------------------------------------------------------------
// Construction: persist_all()
// ---------------------------------------------------------------------------

/// persist_all(events) wraps multiple events verbatim in the Persist variant
#[test]
fn persist_all_wraps_multiple_events_in_persist_variant() {
    // Given
    let events = vec![1u32, 2, 3];

    // When
    let effect = Effect::persist_all(events);

    // Then
    assert_eq!(
        shape_of(&effect),
        Shape::Persist(vec![1u32, 2, 3]),
        "persist_all(vec) must return Persist(vec)"
    );
}

/// persist_all([]) with an empty slice returns Persist with no events (not None)
#[test]
fn persist_all_with_empty_vec_returns_persist_with_no_events() {
    // Given / When
    let effect = Effect::<u32>::persist_all(vec![]);

    // Then: Persist([]) — not None, the variant itself carries the intent to persist
    assert_eq!(
        shape_of(&effect),
        Shape::Persist(vec![]),
        "persist_all([]) must return Persist([]), not None"
    );
}

// ---------------------------------------------------------------------------
// Construction: apply_only()
// ---------------------------------------------------------------------------

/// apply_only(e) wraps a single event in the Apply variant
#[test]
fn apply_only_wraps_single_event_in_apply_variant() {
    // Given
    let event = 7u32;

    // When
    let effect = Effect::apply_only(event);

    // Then
    assert_eq!(
        shape_of(&effect),
        Shape::Apply(vec![7u32]),
        "apply_only(e) must return Apply([e])"
    );
}

// ---------------------------------------------------------------------------
// Construction: tell()
// ---------------------------------------------------------------------------

/// tell(proxy, msg) returns the Side variant; type-safety enforced at compile time
/// via P: Process + Receive<M>
#[tokio::test]
async fn tell_returns_side_variant() {
    // Given: a running process system and a spawned target process
    let system = ProcessSystem::new().await;
    let proxy = system.spawn(Props::new(|| TestProcess)).await;

    // When: building a tell effect
    let effect = Effect::<()>::tell(proxy, TestMsg);

    // Then: the result is the Side variant
    assert!(
        matches!(effect, Effect::Side(_)),
        "tell(proxy, msg) must return Effect::Side"
    );
}

// ---------------------------------------------------------------------------
// Construction: publish()
// ---------------------------------------------------------------------------

/// publish(stream, msg) returns the Side variant
#[tokio::test]
async fn publish_returns_side_variant() {
    // Given: a running process system with a BoxedMessage stream
    let system = ProcessSystem::new().await;
    let stream = system
        .spawn_stream::<BoxedMessage>(ProcessName::new("effect-test-publish"))
        .await
        .expect("spawn_stream should succeed");

    // When: building a publish effect with a BoxedMessage (T = BoxedMessage for Stream<BoxedMessage>)
    let effect = Effect::<()>::publish(stream, BoxedMessage::new(1u32));

    // Then: the result is the Side variant
    assert!(
        matches!(effect, Effect::Side(_)),
        "publish(stream, msg) must return Effect::Side"
    );
}
