pub mod entity {
    include!(concat!("./test_impl_entity.rs"));
}

pub mod store {
    include!(concat!("test_impl_projector.rs"));
}

pub use self::entity::*;
pub use self::store::*;
use spectroscopy::identifier::ToEntityId;
use spectroscopy::protocol::Projector;
use spectroscopy::Event;
use tokio::time::Instant;

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
    for (seq, event) in events.into_iter().enumerate() {
        store
            .write(
                "counter-1".to_string(),
                seq as i64,
                seq as i64,
                CounterEvent::REGISTRY_KEY.to_string(),
                event.as_bytes()?,
            )
            .await;
    }

    let projector = Projector::new(store);

    let now = Instant::now();

    let counter = projector
        .of::<Counter>(None)
        .projection_to_latest(&"counter-1".to_entity_id())
        .await?;

    let elapsed = now.elapsed();
    println!("Elapsed: {:?} micro sec", elapsed.as_micros());

    let (counter, seq) = counter.ok_or(anyhow::Error::msg("wtf"))?;

    assert_eq!(counter.state, 1);
    assert_eq!(seq, 5);

    Ok(())
}
