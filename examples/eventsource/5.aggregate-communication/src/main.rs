//! Example 5 — Aggregate Communication
//!
//! How one aggregate causes something to happen in another.
//!
//! An aggregate does not reach out.  `Decider::decide` is pure and synchronous:
//! it returns a `Decision` — the facts a command produced and the answer it
//! asks for — and a value cannot tell anybody anything.  What reaches out is a
//! pair:
//!
//! 1. a **saga** subscribed to the first aggregate's stream, which for each
//!    fact it sees persists its own decision that a relay is owed — and can
//!    express nothing else, because a `SagaEffect` has no "send this command"
//!    variant;
//! 2. a **delivery host**, which reads those decisions out of the store, asks
//!    the pure `Saga::dispatch` mapping what each one implies, and `ask`s the
//!    aggregate the decision addressed.
//!
//! Deciding and delivering are deliberately not the same step.  A saga's
//! decision is durable the moment it is journalled, whatever the state of the
//! target; and a delivery that fails is retried by the host, which holds its
//! checkpoint rather than passing the command over — so nothing the saga
//! decided is silently never carried out.
//!
//! # Migrating from `Effect::Side`
//!
//! Earlier versions let `decide` return `Effect::Side(..)`, a fire-and-forget
//! future the activation spawned after the append.  It is gone, and this is
//! what replaces it:
//!
//! | Before | Now |
//! |---|---|
//! | `Ok(Effect::Side(Box::new(TellTargetEffect { target, .. })))` | `SagaEffect::persist(RelayRequested { .. })` in `Saga::handle`, and `Saga::dispatch` saying what that decision implies |
//! | the command carries the target proxy | the decision carries the target's stream key, so a host that never met the deciding incarnation can still address it |
//! | the dispatch is lost if the process dies between append and send | the decision is a row on the saga's own stream, and a host resumes from its checkpoint and delivers it again |
//! | a duplicate activation performs it twice, undetected | delivery is at-least-once by construction, and the receiver recognises a repeat by the operation id the decision carried |
//! | nobody learns that the send failed | a failed delivery holds the host's checkpoint, is retried, and is reported through `tracing` |
//!
//! The behaviour is not merely relocated: `Effect::Side` was "send and pray",
//! and a saga plus a delivery host is the durable version of the same intent.
//!
//! Run with:
//!   cargo run -p eventsource-aggregate-communication

use std::time::Duration;

use tracing::info;

use nitinol_eventsource::system::EventSourceSystem;
use nitinol_persistence::provider::PersistenceProvider;
use nitinol_persistence::store::InMemoryStore;
use nitinol_persistence::AggregateId;
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{SagaDeliveryExt, SagaSystemExt};

use eventsource_aggregate_communication::counter::{Counter, GetCount, Increment, Incremented};
use eventsource_aggregate_communication::saga::RelaySaga;

/// How often the delivery host asks the store what is new.
///
/// A pull consumer trades latency for load on the store; an example wants the
/// latency, so it polls far more eagerly than a deployment would.
const POLL_INTERVAL: Duration = Duration::from_millis(25);

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

    // `EventStore` is stream-keyed, so both counters and the saga's own journal
    // are tenants of the one journal the provider supplies, each under its own
    // key.  `InMemoryStore` also supplies a delivery-checkpoint slot, which is
    // the second store a delivery host needs — one that supplied none would
    // make the `deliver` call below a compile error rather than a host unable
    // to record where it got to.
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps)
        .with_persistence(PersistenceProvider::new(InMemoryStore::default()))
        .build();

    let id_a = AggregateId::new("comm-a");
    let id_b = AggregateId::new("comm-b");
    let proxy_a = system.spawn_aggregate::<Counter>(id_a.clone()).await;
    let proxy_b = system.spawn_aggregate::<Counter>(id_b.clone()).await;

    // `host` is the one verb a saga is started with.  `RelaySaga::correlate`
    // answers the same id for every `Incremented`, so what runs is a manager
    // with a single instance — which is why no `SagaId` is named here.
    //
    // The factory is handed the capabilities the framework resolved for this
    // saga; a relay needs none of them, so it ignores them.  It is called again
    // on a supervised restart, so what it captures must be re-acquirable — B's
    // `AggregateId` is, being the address the decision will carry rather than a
    // reference to a live activation.
    //
    // One `observing` call per source the alphabet declares: A's stream, read
    // as `Incremented`.  `spawn` exists only once every declared source has
    // one.
    let target = id_b.clone();
    let _saga_proxy = system
        .host::<RelaySaga>(move |_components| RelaySaga {
            target: target.clone(),
        })
        .observing::<Incremented>(system.subscription(&id_a))
        .spawn()
        .await;

    // The delivery host: the half that actually reaches B.  Its whole input is
    // the store — the decisions every instance of `RelaySaga` has persisted —
    // so it holds no saga and needs nothing of the saga process; and it reaches
    // B through the same reference `proxy_b` is, which is why B implements
    // nothing of the host's.  Omit this spawn and the saga still decides, still
    // journals, and B is never touched.
    let _delivery = system
        .deliver::<RelaySaga>()
        .with_poll_interval(POLL_INTERVAL)
        .spawn()
        .await;

    // A decides, and answers with what the command asked for: its new value.
    // Nothing about B is decided here, and nothing about B is awaited here.
    let count_a = proxy_a
        .ask(Increment {
            operation: "manual-increment-1".into(),
        })
        .await
        .expect("ask must succeed");
    assert_eq!(count_a, 1, "A must answer with its own new value");
    info!(count_a, "ask(Increment) on A returned");

    // The relay is asynchronous twice over — A's ask returned the moment A's own
    // fact was durable, which is before the saga had seen it, which is in turn
    // before the host had read the decision it led to.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let count_b = loop {
        let c = proxy_b.exec(GetCount).await.expect("exec must succeed");
        if c >= 1 {
            break c;
        }
        if std::time::Instant::now() >= deadline {
            panic!("the relay must drive an Increment into B within 5 seconds");
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    };
    assert_eq!(
        count_b, 1,
        "one fact of A means one increment of B: a redelivered relay names the \
         same operation, and B recognises it and writes nothing"
    );

    info!(count_b, "B counter after the relayed Increment");
    info!("example finished successfully");
}
