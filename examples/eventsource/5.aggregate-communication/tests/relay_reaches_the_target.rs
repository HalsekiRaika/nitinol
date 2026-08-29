// Aggregate-to-aggregate communication, as the migration guide states it.
//
// The behaviour that used to belong to `Effect::Side` — one aggregate causing a
// command to reach another — now belongs to a saga, and this is what that has
// to keep true:
//
//   * A's `ask` answers with A's own decision.  It says nothing about B, and it
//     does not wait for B: the relay is a consequence of A's fact, not part of
//     the decision that produced it.
//   * The command does reach B, without anyone asking B for it.
//   * A's own stream holds only A's fact.  The relay is the saga's business and
//     leaves no trace in the aggregate that triggered it.

use std::sync::Arc;
use std::time::Duration;

use futures_util::TryStreamExt;

use nitinol_eventsource::system::EventSourceSystem;
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::{AggregateId, LoadQuery};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::SagaDefaultStoreExt;

use eventsource_aggregate_communication::codec::JsonCodec;
use eventsource_aggregate_communication::counter::{Counter, GetCount, Increment};
use eventsource_aggregate_communication::saga::RelaySaga;

/// How long the relay is given before it is called a failure.
///
/// Generous on purpose: the relay crosses a subscription, a journal append and a
/// mailbox, so a tight bound would measure the machine rather than the
/// behaviour.  A broken relay never arrives at all, so the test fails on the
/// deadline rather than passing slowly.
const RELAY_DEADLINE: Duration = Duration::from_secs(3);

/// How many events `id` holds.
async fn stored_len(store: &Arc<dyn EventStore>, id: &AggregateId) -> usize {
    let loaded: Vec<_> = store
        .load(LoadQuery::by_stream(id))
        .await
        .expect("load must succeed")
        .try_collect()
        .await
        .expect("collecting the stream must succeed");
    loaded.len()
}

#[tokio::test]
async fn a_fact_of_one_aggregate_reaches_another_as_a_command() {
    // Given
    let ps = ProcessSystem::new().await;
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let system = EventSourceSystem::builder(ps)
        .with_codec::<JsonCodec>()
        .with_event_store(Arc::clone(&store))
        .build();

    let id_a = AggregateId::new("relay-test-a");
    let id_b = AggregateId::new("relay-test-b");
    let proxy_a = system.spawn_aggregate::<Counter>(id_a.clone()).await;
    let proxy_b = system.spawn_aggregate::<Counter>(id_b.clone()).await;

    let target = proxy_b.clone();
    let _saga = system
        .spawn_saga(RelaySaga::instance_id(), move || RelaySaga {
            target: target.clone(),
        })
        .subscribed_to(system.subscription(&id_a))
        .spawn()
        .await;

    // When
    let count_a = proxy_a.ask(Increment).await.expect("ask must succeed");

    // Then: the answer is A's, and only A's
    assert_eq!(
        count_a, 1,
        "ask must answer with the value A's own decision stated"
    );

    // Then: the relay arrives, unasked
    let deadline = std::time::Instant::now() + RELAY_DEADLINE;
    let count_b = loop {
        let observed = proxy_b.exec(GetCount).await.expect("exec must succeed");
        if observed >= 1 {
            break observed;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the saga must relay an Increment into B within {RELAY_DEADLINE:?}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    assert_eq!(count_b, 1, "B must have been incremented exactly once");

    // Then: A's stream holds A's fact and nothing about the relay
    assert_eq!(
        stored_len(&store, &id_a).await,
        1,
        "the relay must leave no trace in the stream of the aggregate that triggered it"
    );
}
