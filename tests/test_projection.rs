pub mod entity {
    include!(concat!("./test_impl_entity.rs"));
}

pub mod store {
    include!(concat!("test_impl_projector.rs"));
}

use tokio::time::Instant;
use spectrum::Event;
use spectrum::identifier::ToEntityId;
use spectrum::protocol::Projector;
pub use self::entity::*;
pub use self::store::*;

#[tokio::test]
async fn main() -> anyhow::Result<()> {
    let store = InMemoryEventStore::default();
    let events = vec![
        CounterEvent::Increased,
        CounterEvent::Increased,
        CounterEvent::Decreased,
        CounterEvent::Increased,
        CounterEvent::Decreased,
    ];
    let mut seq = 0;
    for event in events {
        store.write("counter-1".to_string(), seq, CounterEvent::REGISTRY_KEY.to_string(), event.as_bytes()?).await;
        seq += 1;
    }

    let projector = Projector::new(store);

    let now = Instant::now();

    let counter = projector.of::<Counter>(None)
        .projection_to_latest(&"counter-1".to_entity_id())
        .await?;

    let elapsed = now.elapsed();
    println!("Elapsed: {:?}ms", elapsed.as_micros());

    let (counter, seq) = counter.ok_or(anyhow::Error::msg("wtf"))?;


    assert_eq!(counter.state, 1);
    assert_eq!(seq, 5);

    Ok(())
}