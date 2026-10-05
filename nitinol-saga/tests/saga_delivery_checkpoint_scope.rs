//! What a checkpoint key is, now that two different consumers write them.
//!
//! ```gherkin
//!   Scenario: one store, two kinds of consumer
//!     Given one checkpoint store
//!     When a projection and a delivery partition each record progress
//!     Then each reads back its own position and neither reads the other's
//!
//!   Scenario: a key that could be mistaken for the other kind
//!     Given a delivery partition's key
//!     When a projection is named the same text
//!     Then naming it is refused
//! ```
//!
//! The second scenario is what makes the separation a property of the types
//! rather than of a convention: a projection identifier refuses the framework's
//! reserved namespace when it is constructed, and a delivery scope renders
//! inside it, so the two sets of keys cannot meet. The provider's table is
//! shared because there is nothing to keep apart.
//!
//! A projection identifier still renders verbatim, so rows written before a
//! delivery host existed keep the keys they were written under.

use std::sync::Arc;

use nitinol_persistence::store::{CheckpointScope, CheckpointStore, InMemoryCheckpointStore};
use nitinol_persistence::{ProjectionId, RESERVED_NAMESPACE};
use nitinol_saga::{SagaDeliveryId, SagaDeliveryScope};

/// Why an in-memory checkpoint call cannot fail: it reads and writes a map
/// behind a lock no holder panics under.
const STORE_ANSWERS: &str = "an in-memory checkpoint store answers every call";

fn delivery_scope(id: &str, partition: usize) -> SagaDeliveryScope {
    SagaDeliveryScope::new(&SagaDeliveryId::new(id), partition)
}

#[test]
fn a_projection_identifier_still_renders_verbatim() {
    assert_eq!(
        ProjectionId::new("order-totals").checkpoint_key(),
        "order-totals",
        "a stored projection row is found under the identifier's own text, so \
         generalising the key must not prefix or decorate it"
    );
}

#[test]
fn a_delivery_scope_renders_inside_the_framework_namespace() {
    let key = delivery_scope("orders", 0).checkpoint_key().to_owned();

    assert!(
        key == RESERVED_NAMESPACE || key.starts_with(&format!("{RESERVED_NAMESPACE}.")),
        "a delivery scope renders inside the framework's own namespace so no \
         projection identifier can render the same key; rendered {key:?}"
    );
}

#[test]
fn a_projection_cannot_be_named_a_delivery_scope_key() {
    let key = delivery_scope("orders", 0).checkpoint_key().to_owned();
    let named_as = key.clone();

    let outcome = std::panic::catch_unwind(move || ProjectionId::new(named_as));

    assert!(
        outcome.is_err(),
        "the two key spaces are disjoint by construction: a projection named \
         {key:?} must be refused rather than silently share a delivery \
         partition's row"
    );
}

#[test]
fn a_key_names_one_identifier_and_one_partition() {
    assert_eq!(
        delivery_scope("orders", 1).checkpoint_key(),
        delivery_scope("orders", 1).checkpoint_key(),
        "the same identifier and partition render the same key, so a restarted \
         partition finds the row it wrote"
    );

    assert_ne!(
        delivery_scope("orders", 0).checkpoint_key(),
        delivery_scope("orders", 1).checkpoint_key(),
        "two partitions of one host keep independent progress, so their keys \
         differ"
    );

    assert_ne!(
        delivery_scope("orders", 1).checkpoint_key(),
        delivery_scope("orders.1", 0).checkpoint_key(),
        "the rendering is injective: an identifier ending in what could be read \
         as a partition number must not collide with the pair that would render \
         the same text"
    );
}

#[tokio::test]
async fn one_store_serves_both_kinds_without_either_reading_the_other() {
    let store = Arc::new(InMemoryCheckpointStore::default());

    // The same text on both sides, so nothing but the rendering keeps them
    // apart.
    let projection = ProjectionId::new("orders");
    let partition = delivery_scope("orders", 0);

    store
        .save(&projection, 11, None)
        .await
        .expect(STORE_ANSWERS);
    store.save(&partition, 22, None).await.expect(STORE_ANSWERS);

    assert_eq!(
        store.load(&projection).await.expect(STORE_ANSWERS),
        Some(11),
        "the projection reads back its own position"
    );
    assert_eq!(
        store.load(&partition).await.expect(STORE_ANSWERS),
        Some(22),
        "the delivery partition reads back its own position from the same \
         store, so the two consumers share one stored progress cache without \
         sharing a key"
    );
}

#[tokio::test]
async fn a_scope_with_no_row_yet_reads_as_absent() {
    let store = Arc::new(InMemoryCheckpointStore::default());

    assert_eq!(
        store
            .load(&delivery_scope("orders", 3))
            .await
            .expect(STORE_ANSWERS),
        None,
        "a partition that has never saved has no position, which is what lets a \
         host tell `never started` from `started at zero`"
    );
}
