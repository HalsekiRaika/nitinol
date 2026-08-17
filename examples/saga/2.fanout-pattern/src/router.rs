//! Where the fan-out's children live, and how a command finds one.
//!
//! A fan-out addresses children that may not be running — the first delivery
//! creates them, and a replay after a crash addresses a set that is partly
//! resident and partly gone.  Routing is therefore not "hold 32 proxies": it is
//! "given a stream key, produce the one process that owns it".  That answer has
//! a single owner here so no call site can invent a second process for a stream
//! that already has one.

use std::collections::HashMap;
use std::sync::Arc;

use futures_core::future::BoxFuture;
use tokio::sync::Mutex;

use nitinol_eventsource::system::EventSourceSystem;
use nitinol_eventsource::{AggregateProxy, AggregateTellTarget, Decider, TellError};
use nitinol_persistence::store::EventStore;
use nitinol_persistence::AggregateId;

use crate::codec::JsonCodec;
use crate::item::Item;

/// Owns the child processes of one fan-out incarnation.
///
/// A key resolves to the same [`AggregateProxy`] for the registry's lifetime.
/// Two processes over one stream would both replay it, both believe the item
/// uncreated, and both try to write its genesis record — so the second one's
/// append would conflict and take that child down.  Resolving through one
/// registry is what prevents that.
pub struct ItemRegistry {
    system: Arc<EventSourceSystem<JsonCodec>>,
    store: Arc<dyn EventStore>,
    /// Held across the spawn `await` so two concurrent resolutions of the same
    /// key cannot both observe it as absent.
    resident: Mutex<HashMap<String, AggregateProxy<Item>>>,
}

impl ItemRegistry {
    pub fn new(system: Arc<EventSourceSystem<JsonCodec>>, store: Arc<dyn EventStore>) -> Self {
        Self {
            system,
            store,
            resident: Mutex::new(HashMap::new()),
        }
    }

    /// The process that owns the stream `item`, spawning and replaying it on
    /// first use.
    pub async fn resolve(&self, item: &str) -> AggregateProxy<Item> {
        let mut resident = self.resident.lock().await;
        if let Some(proxy) = resident.get(item) {
            return proxy.clone();
        }
        let proxy = self
            .system
            .spawn_aggregate::<Item>(AggregateId::new(item), Arc::clone(&self.store))
            .await;
        resident.insert(item.to_owned(), proxy.clone());
        proxy
    }

    /// A tell target that resolves `item` at dispatch time.
    pub fn target(self: &Arc<Self>, item: &str) -> ItemTarget {
        ItemTarget {
            registry: Arc::clone(self),
            item: item.to_owned(),
        }
    }
}

/// A tell target named by stream key rather than by a live proxy.
///
/// The saga builds one of these per child while deciding, and the outbox
/// dispatches it later — possibly after the incarnation that decided is gone.
/// Resolving inside [`AggregateTellTarget::tell`] rather than at construction
/// is what makes that work: the same target value is also what the
/// crash-restart factory reconstructs from a `TellRequested` marker, where no
/// proxy from the original incarnation survives.
#[derive(Clone)]
pub struct ItemTarget {
    registry: Arc<ItemRegistry>,
    item: String,
}

impl AggregateTellTarget<Item> for ItemTarget {
    fn tell<C>(&'_ self, cmd: C) -> BoxFuture<'_, Result<(), TellError>>
    where
        Item: Decider<C>,
        C: Send + Sync + 'static,
    {
        Box::pin(async move {
            let proxy = self.registry.resolve(&self.item).await;
            proxy.tell(cmd).await
        })
    }

    fn aggregate_id_str(&self) -> &str {
        &self.item
    }
}
