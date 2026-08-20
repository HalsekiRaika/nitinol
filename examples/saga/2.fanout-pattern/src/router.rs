//! Where the fan-out's payslips live, and how a command finds one.
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
use crate::payslip::Payslip;

/// Owns the payslip processes of one fan-out incarnation.
///
/// A key resolves to the same [`AggregateProxy`] for the registry's lifetime.
/// Two processes over one stream would both replay it, both believe the payslip
/// unissued, and both try to write its genesis record — so the second one's
/// append would conflict and take that payslip down.  Resolving through one
/// registry is what prevents that.
///
/// The `store` handed in here is the same instance the payroll run and the saga
/// journal use: [`EventStore`] is keyed by stream, so every payslip is a tenant
/// of it under its own key.
pub struct PayslipRegistry {
    system: Arc<EventSourceSystem<JsonCodec>>,
    store: Arc<dyn EventStore>,
    /// Held across the spawn `await` so two concurrent resolutions of the same
    /// key cannot both observe it as absent.
    resident: Mutex<HashMap<String, AggregateProxy<Payslip>>>,
}

impl PayslipRegistry {
    pub fn new(system: Arc<EventSourceSystem<JsonCodec>>, store: Arc<dyn EventStore>) -> Self {
        Self {
            system,
            store,
            resident: Mutex::new(HashMap::new()),
        }
    }

    /// The process that owns the stream `payslip`, spawning and replaying it on
    /// first use.
    pub async fn resolve(&self, payslip: &str) -> AggregateProxy<Payslip> {
        let mut resident = self.resident.lock().await;
        if let Some(proxy) = resident.get(payslip) {
            return proxy.clone();
        }
        let proxy = self
            .system
            .spawn_aggregate::<Payslip>(AggregateId::new(payslip), Arc::clone(&self.store))
            .await;
        resident.insert(payslip.to_owned(), proxy.clone());
        proxy
    }

    /// A tell target that resolves `payslip` at dispatch time.
    pub fn target(self: &Arc<Self>, payslip: &str) -> PayslipTarget {
        PayslipTarget {
            registry: Arc::clone(self),
            payslip: payslip.to_owned(),
        }
    }
}

/// A tell target named by stream key rather than by a live proxy.
///
/// The saga builds one of these per payslip while deciding, and the outbox
/// dispatches it later — possibly after the incarnation that decided is gone.
/// Resolving inside [`AggregateTellTarget::tell`] rather than at construction
/// is what makes that work: the same target value is also what the
/// crash-restart factory reconstructs from a `TellRequested` marker, where no
/// proxy from the original incarnation survives.
#[derive(Clone)]
pub struct PayslipTarget {
    registry: Arc<PayslipRegistry>,
    payslip: String,
}

impl AggregateTellTarget<Payslip> for PayslipTarget {
    fn tell<C>(&'_ self, cmd: C) -> BoxFuture<'_, Result<(), TellError>>
    where
        Payslip: Decider<C>,
        C: Send + Sync + 'static,
    {
        Box::pin(async move {
            let proxy = self.registry.resolve(&self.payslip).await;
            proxy.tell(cmd).await
        })
    }

    fn aggregate_id_str(&self) -> &str {
        &self.payslip
    }
}
