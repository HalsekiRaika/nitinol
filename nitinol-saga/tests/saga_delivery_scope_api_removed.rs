//! The delivery host's own key type and checkpoint verb leave no surface
//! behind: a user who still names either must fail to compile.
//!
//! ```gherkin
//!   Scenario: a reference to a removed name is a compile error
//!     Given user code naming SagaDeliveryScope or advance_checkpoint
//!     When it is compiled against this crate
//!     Then compilation fails — nothing is deprecated and no hidden compatible
//!          shim answers for the name
//! ```
//!
//! `SagaDeliveryScope` rendered a key of its own and argued its own
//! injectivity. That argument belongs in one place, so the key is a concrete
//! `nitinol_persistence::store::OffsetScope` built through the framework-owned
//! constructor, and what this crate still owns is the triple it hands over —
//! pinned positively by `saga_delivery_offset_scope.rs` and
//! `nitinol-persistence/tests/offset_scope.rs`. `advance_offset` is the
//! operator verb, pinned by `saga_delivery_offset_advance_escape.rs`. This file
//! pins that the shapes they replace are gone rather than kept beside them.
//!
//! Two mechanisms, each the one that fits what is being settled, both taken
//! from the `*_api_removed.rs` tests already in this directory:
//!
//! 1. **Released type names** — the crate's public namespace and a local
//!    `probe` namespace are both glob-imported and the bare name is used. Once
//!    the crate stops exporting it the name has a single source and resolves to
//!    the probe; while it still exports it the two globs collide and this binary
//!    fails to build. The probe type carries an `owner` field the released one
//!    never had, so resolving the ambiguity in the crate's favour does not
//!    rescue the build either.
//! 2. **Released methods** — a blanket extension trait claims the name with a
//!    signature the released method could never accept. `advance_checkpoint`
//!    was an *inherent* method, and an inherent method wins method resolution
//!    over a trait one, so while it exists the call below is checked against
//!    **it** and fails on its arguments. Once the name is `advance_offset`, the
//!    blanket method is the only candidate and resolves.
//!
//! Run with:
//! `cargo test -p nitinol-saga --test saga_delivery_scope_api_removed`.

use async_trait::async_trait;
use bytes::Bytes;

use nitinol_eventsource::codec::Codec;
use nitinol_eventsource::system::EventSourceSystem;
use nitinol_eventsource::Event;
use nitinol_persistence::provider::PersistenceProvider;
use nitinol_persistence::store::InMemoryStore;
use nitinol_persistence::{EventType, Family, TypeName};
use nitinol_runtime::ProcessSystem;

/// What a downstream crate is free to define once the names are released.
mod probe {
    /// What a released name means to a downstream crate: nothing the framework
    /// decides. The `owner` field is what no released type ever had.
    pub struct SagaDeliveryScope {
        pub owner: &'static str,
    }

    /// What a probed call answers with once the framework stops claiming the
    /// name.
    pub struct Declined {
        pub label: &'static str,
    }

    /// Claims the withdrawn operator verb on the delivery handle.
    pub trait NoAdvanceCheckpointVerb {
        fn advance_checkpoint(&self, label: &'static str) -> Declined {
            Declined { label }
        }
    }

    impl<T> NoAdvanceCheckpointVerb for T {}
}

// The crate glob is the probe, not a source of names: it contributes nothing
// only while the crate exports none of the released names. The moment it
// exports one, this glob collides with `probe::*` and the use sites below stop
// resolving.
#[allow(unused_imports)]
use nitinol_saga::*;
// A glob, not a named import: a named import would shadow the crate's export
// instead of colliding with it, and the collision is the whole mechanism.
#[allow(unused_imports)]
use probe::*;

const PROBE_OWNER: &str = "downstream-probe";
const PROBE_LABEL: &str = "downstream-label";

// A minimal saga, so a delivery host exists to call the probed verb on.

struct UnitCodec;

impl Codec<Signal> for UnitCodec {
    type Error = std::convert::Infallible;

    fn encode(_value: &Signal) -> Result<Bytes, Self::Error> {
        Ok(Bytes::new())
    }

    fn decode(_payload: &[u8]) -> Result<Signal, Self::Error> {
        Ok(Signal)
    }
}

impl Codec<Decided> for UnitCodec {
    type Error = std::convert::Infallible;

    fn encode(_value: &Decided) -> Result<Bytes, Self::Error> {
        Ok(Bytes::new())
    }

    fn decode(_payload: &[u8]) -> Result<Decided, Self::Error> {
        Ok(Decided)
    }
}

#[derive(Clone)]
struct Signal;

impl Event for Signal {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("saga_delivery_scope_api_removed"),
        TypeName::new("Signal"),
    );
    type Codec = UnitCodec;
}

#[derive(Clone)]
struct Decided;

impl Event for Decided {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("saga_delivery_scope_api_removed"),
        TypeName::new("Decided"),
    );
    type Codec = UnitCodec;
}

#[derive(SagaSubscribe)]
enum Observed {
    Signal(Signal),
}

/// Correlation rule of [`ProbeSaga`]: the single process instance every
/// upstream event in this file belongs to.
const PROBE_SAGA_ID: &str = "delivery-scope-api-removed";

struct ProbeSaga;

#[async_trait]
impl Saga for ProbeSaga {
    type Subscribe = Observed;
    type Event = Decided;
    type Error = std::convert::Infallible;

    fn correlate(_input: &Self::Subscribe) -> Option<SagaId> {
        Some(SagaId::new(PROBE_SAGA_ID))
    }

    async fn handle(
        &mut self,
        _input: Self::Subscribe,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        Ok(SagaEffect::None)
    }
}

/// A system over a store that supplies every slot, the one a delivery host
/// records its offsets in included.
async fn system() -> EventSourceSystem<InMemoryStore> {
    EventSourceSystem::builder(ProcessSystem::new().await)
        .with_persistence(PersistenceProvider::new(InMemoryStore::default()))
        .build()
}

/// Given both namespaces are glob-imported, When the bare `SagaDeliveryScope`
/// name is used, Then it denotes the downstream probe definition — the key a
/// partition records under is an `OffsetScope`, and what this crate owns is the
/// triple it is built from.
#[test]
fn the_delivery_scope_type_name_belongs_to_a_downstream_definition() {
    assert_eq!(
        SagaDeliveryScope { owner: PROBE_OWNER }.owner,
        PROBE_OWNER,
        "`nitinol-saga` must not export a key type of its own: a second type \
         rendering keys means a second place the injectivity and the namespace \
         separation have to be argued, and the argument belongs to `OffsetScope`"
    );
}

/// Given a blanket extension trait that claims the withdrawn operator verb,
/// When it is called on a running host's handle, Then it resolves to the
/// extension — what an operator moves on is the partition's offset.
#[tokio::test]
async fn the_withdrawn_advance_verb_belongs_to_a_downstream_definition() {
    let host = system().await.deliver::<ProbeSaga>().spawn().await;

    assert_eq!(
        host.advance_checkpoint(PROBE_LABEL).label,
        PROBE_LABEL,
        "a delivery handle must offer `advance_offset`: the position an \
         operator moves on is an offset in the journal's order, and the verb \
         names what it moves"
    );
}
