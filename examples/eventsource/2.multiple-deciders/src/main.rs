//! Example 2 — Multiple Deciders
//!
//! A single `Wallet` aggregate implements two `Decider<C>` impls:
//! - `Decider<Deposit>` — always succeeds
//! - `Decider<Withdraw>` — rejects with `InsufficientFunds` when balance is too low
//!
//! Run with:
//!   cargo run -p eventsource-multiple-deciders

use std::sync::Arc;

use tracing::info;

use nitinol_eventsource::{system::EventSourceSystem, EventPersistor};
use nitinol_persistence::{store::InMemoryEventStore, AggregateId};
use nitinol_runtime::ProcessSystem;

use eventsource_multiple_deciders::codec::JsonCodec;
use eventsource_multiple_deciders::wallet::{Deposit, GetBalance, Wallet, Withdraw};

fn init_tracing() {
    use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(filter)
        .init();
}

#[tokio::main]
async fn main() {
    init_tracing();

    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();
    let event_ref = EventPersistor::spawn(
        system.process_system(),
        Arc::new(InMemoryEventStore::default()),
    )
    .await;

    let proxy = system
        .spawn_aggregate::<Wallet>(AggregateId::new("wallet-1"), event_ref)
        .await;

    // Deposit 100
    let events = proxy.ask(Deposit { amount: 100 }).await.expect("deposit failed");
    info!(?events, "deposit returned");

    let balance = proxy.exec(GetBalance).await.expect("exec failed");
    info!(balance, "balance after deposit");
    assert_eq!(balance, 100);

    // Withdraw 40
    proxy.ask(Withdraw { amount: 40 }).await.expect("withdraw failed");
    let balance = proxy.exec(GetBalance).await.expect("exec failed");
    info!(balance, "balance after withdrawal");
    assert_eq!(balance, 60);

    // Attempt to over-withdraw — expect a rejection
    let result = proxy.ask(Withdraw { amount: 200 }).await;
    assert!(result.is_err(), "over-withdrawal must be rejected");
    info!("over-withdrawal correctly rejected");

    info!("example finished successfully");
}
