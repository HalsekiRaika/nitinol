//! Outbox push delivery and the persistent dead letter queue leave no surface
//! behind: a user who still names one must fail to compile.
//!
//! ```gherkin
//!   Scenario: a reference to a removed API is a compile error
//!     Given user code naming TellIntent / with_crash_restart_factory /
//!           DeadLetterQueue / on_tell_failed
//!     When it is compiled against this crate
//!     Then compilation fails — nothing is deprecated and no hidden compatible
//!          shim answers for the name
//! ```
//!
//! Three mechanisms, each the one that fits what is being settled, all taken
//! from the `*_api_removed.rs` tests already in this directory:
//!
//! 1. **Released type names** — the crate's public namespace and a local
//!    `probe` namespace are both glob-imported and the bare names are used.
//!    Once the crate stops exporting them the names have a single source and
//!    resolve to the probe; while it still exports one the two globs collide
//!    and this binary fails to build. Each probe type carries an `owner` field
//!    the released types never had, so resolving the ambiguity in the crate's
//!    favour does not rescue the build either.
//! 2. **Released builder and accessor methods** — a blanket extension trait
//!    claims the name with a signature the released method could never accept.
//!    An inherent method wins method resolution over a trait one, so while the
//!    inherent method exists the call below is checked against *it* and fails
//!    on its argument. Once it is gone the blanket method is the only candidate
//!    and resolves.
//! 3. **A released `Saga` trait item** — `on_tell_failed` is probed by an
//!    extension trait claiming the same name for the same receiver, the way
//!    `saga_trait_snapshot_methods_removed.rs` probes `snapshot`.
//!
//! Two removals are settled by a type that cannot be named rather than by an
//! assertion of their own, because neither has a value this test could call
//! them on:
//!
//! - `SagaManagerProxy::dead_letter_queue` returns a `DeadLetterQueue`, and
//!   `SagaManagerProxy` has no public constructor. A method cannot outlive its
//!   own return type, so `the_dlq_type_names_belong_to_downstream_definitions`
//!   settles it — the same reasoning
//!   `saga_codec_injection_api_removed.rs` uses for `with_codec`.
//! - `SagaEffect::Persist`'s `tells` field is settled by `TellIntent`: a field
//!   of `Vec<TellIntent>` cannot survive a type that no longer exists, and the
//!   shape the variant does carry is pinned positively by
//!   `saga_combine_persist_single_append.rs`.
//!
//! The replacement is not an alternative spelling of the same mechanism: a
//! saga's decisions reach their targets through `Saga::dispatch` and the
//! delivery host (`SagaDeliveryProps`), which is pinned positively by the
//! `saga_delivery_*.rs` tests. Transport failure gets no saga-facing hook at
//! all.
//!
//! Run with:
//! `cargo test -p nitinol-saga --test saga_outbox_dlq_api_removed`.

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;

use nitinol_eventsource::codec::Codec;
use nitinol_eventsource::Event;
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::{EventType, Family, TypeName};

/// What a downstream crate is free to define once the names are released.
mod probe {
    /// What a released name means to a downstream crate: nothing the framework
    /// decides. The `owner` field is what no released type ever had.
    macro_rules! released_type {
        ($($name:ident),+ $(,)?) => {
            $(
                pub struct $name {
                    pub owner: &'static str,
                }
            )+
        };
    }

    released_type!(
        TellIntent,
        DeadLetterEvent,
        DeadLetterEntry,
        DeadLetterQuery,
        DeadLetterQueue,
        DeadLetterQueueError,
        EnqueueDecision,
        EnqueuePolicy,
        SagaFailure,
        SourceContext,
    );

    /// What a probed call answers with once the framework stops claiming the
    /// name.
    pub struct Declined {
        pub label: &'static str,
    }

    /// Claims the outbox push wiring on both saga builders.
    pub trait NoOutboxPushWiring {
        fn with_crash_restart_factory(&self, label: &'static str) -> Declined {
            Declined { label }
        }
    }

    impl<T> NoOutboxPushWiring for T {}

    /// Claims the dead-letter wiring on both saga builders.
    pub trait NoDeadLetterWiring {
        fn with_dead_letter_subscriber(&self, label: &'static str) -> Declined {
            Declined { label }
        }

        fn with_enqueue_policy(&self, label: &'static str) -> Declined {
            Declined { label }
        }
    }

    impl<T> NoDeadLetterWiring for T {}

    /// Claims the tell-failure report `SagaContext` used to carry.
    pub trait NoFailedTellReport {
        fn failed_tell_ids(&self, label: &'static str) -> Declined {
            Declined { label }
        }
    }

    impl<T> NoFailedTellReport for T {}

    /// Claims the two tell constructors on `SagaEffect`.
    ///
    /// Associated functions rather than methods, because that is what the
    /// released pair were: `SagaEffect::tell(target, cmd)` and
    /// `SagaEffect::tell_intent(intent)`.
    pub trait NoTellConstructors {
        fn tell(label: &'static str) -> Declined {
            Declined { label }
        }

        fn tell_intent(label: &'static str) -> Declined {
            Declined { label }
        }
    }

    impl<T> NoTellConstructors for T {}
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

// A minimal saga, so a builder value and a receiver exist to call the probed
// items on.

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

impl Codec<Noted> for UnitCodec {
    type Error = std::convert::Infallible;

    fn encode(_value: &Noted) -> Result<Bytes, Self::Error> {
        Ok(Bytes::new())
    }

    fn decode(_payload: &[u8]) -> Result<Noted, Self::Error> {
        Ok(Noted)
    }
}

#[derive(Clone)]
struct Signal;

impl Event for Signal {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("saga_outbox_dlq_api_removed"),
        TypeName::new("Signal"),
    );
    type Codec = UnitCodec;
}

#[derive(Clone)]
struct Noted;

impl Event for Noted {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("saga_outbox_dlq_api_removed"),
        TypeName::new("Noted"),
    );
    type Codec = UnitCodec;
}

#[derive(SagaSubscribe)]
enum Observed {
    Signal(Signal),
}

/// Correlation rule of [`ProbeSaga`]: the single process instance every
/// upstream event in this file belongs to.
const PROBE_SAGA_ID: &str = "outbox-dlq-api-removed";

struct ProbeSaga;

#[async_trait]
impl Saga for ProbeSaga {
    type Subscribe = Observed;
    type Event = Noted;
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

/// Claims the `on_tell_failed` name for a saga receiver. A downstream
/// extension trait may only do this once `Saga` has stopped claiming it.
///
/// `&mut self` on purpose: the released trait item took `&mut self` too, and
/// method resolution walks the autoref steps in order (`&T` before `&mut T`)
/// rather than looking at arity. A `&self` probe would therefore be picked
/// *ahead* of a surviving `&mut self` trait item and this test would pass while
/// the hook was still there. Claiming the same receiver form makes both
/// candidates applicable at the same step, which is the ambiguity (E0034) that
/// fails the build while `Saga::on_tell_failed` exists.
trait OnTellFailedProbe {
    fn on_tell_failed(&mut self, label: &'static str) -> Declined;
}

impl OnTellFailedProbe for ProbeSaga {
    fn on_tell_failed(&mut self, label: &'static str) -> Declined {
        Declined { label }
    }
}

fn store() -> Arc<dyn EventStore> {
    Arc::new(InMemoryEventStore::default())
}

fn props() -> SagaProps<ProbeSaga> {
    SagaProps::<ProbeSaga>::new(SagaId::new(PROBE_SAGA_ID), store(), |_components| ProbeSaga)
}

fn manager_props() -> SagaManagerProps<ProbeSaga> {
    SagaManagerProps::<ProbeSaga>::new(store(), |_components| ProbeSaga)
}

/// Given both namespaces are glob-imported, When the bare `TellIntent` name is
/// used, Then it denotes the downstream probe definition — no outbox marker
/// carries a command's bytes any more, so nothing names the intent to send
/// one.
#[test]
fn the_tell_intent_name_belongs_to_downstream_definitions() {
    let intent = TellIntent { owner: PROBE_OWNER };

    assert_eq!(
        intent.owner, PROBE_OWNER,
        "`TellIntent` must not be exported by `nitinol-saga` any more: a saga's \
         decisions reach their targets through `Saga::dispatch` and the \
         delivery host, not through a persisted command copy"
    );
}

/// Given both namespaces are glob-imported, When each bare dead-letter name is
/// used, Then every one of them denotes the downstream probe definition — the
/// persistent DLQ is withdrawn rather than redefined.
#[test]
fn the_dlq_type_names_belong_to_downstream_definitions() {
    let owners = [
        DeadLetterEvent { owner: PROBE_OWNER }.owner,
        DeadLetterEntry { owner: PROBE_OWNER }.owner,
        DeadLetterQuery { owner: PROBE_OWNER }.owner,
        DeadLetterQueue { owner: PROBE_OWNER }.owner,
        DeadLetterQueueError { owner: PROBE_OWNER }.owner,
        EnqueueDecision { owner: PROBE_OWNER }.owner,
        EnqueuePolicy { owner: PROBE_OWNER }.owner,
        SagaFailure { owner: PROBE_OWNER }.owner,
        SourceContext { owner: PROBE_OWNER }.owner,
    ];

    assert!(
        owners.iter().all(|owner| *owner == PROBE_OWNER),
        "every dead-letter name must be released by `nitinol-saga`: a store of \
         type-less bytes is withdrawn, not re-specified, and `DeadLetterQueue` \
         going with it is what settles `SagaManagerProxy::dead_letter_queue` — \
         a method cannot outlive its own return type; got {owners:?}"
    );
}

/// Given a blanket extension trait that claims the `with_crash_restart_factory`
/// name, When it is called on each saga builder, Then it resolves to the
/// extension — neither builder has an inherent method of that name to win
/// resolution.
#[test]
fn the_crash_restart_factory_name_belongs_to_downstream_definitions() {
    assert_eq!(
        (
            props().with_crash_restart_factory(PROBE_LABEL).label,
            manager_props()
                .with_crash_restart_factory(PROBE_LABEL)
                .label,
        ),
        (PROBE_LABEL, PROBE_LABEL),
        "neither `SagaProps` nor `SagaManagerProps` may take a crash-restart \
         factory: replay is a fold and a reconnect, so there is no pending \
         command for a user-written hook to rebuild"
    );
}

/// Given a blanket extension trait that claims the two dead-letter builder
/// names, When each is called on each saga builder, Then all four calls resolve
/// to the extension.
#[test]
fn the_dead_letter_builder_names_belong_to_downstream_definitions() {
    let labels = [
        props().with_dead_letter_subscriber(PROBE_LABEL).label,
        props().with_enqueue_policy(PROBE_LABEL).label,
        manager_props()
            .with_dead_letter_subscriber(PROBE_LABEL)
            .label,
        manager_props().with_enqueue_policy(PROBE_LABEL).label,
    ];

    assert!(
        labels.iter().all(|label| *label == PROBE_LABEL),
        "no saga builder may take a dead-letter subscriber or an enqueue \
         policy: there is no queue left to subscribe to or filter for; got \
         {labels:?}"
    );
}

/// Given a blanket extension trait that claims the `failed_tell_ids` name, When
/// it is called on a `SagaContext`, Then it resolves to the extension —
/// `SagaContext` no longer reports transport failures to `handle`.
#[test]
fn the_failed_tell_ids_name_belongs_to_downstream_definitions() {
    let ctx = SagaContext::test_context(SagaId::new(PROBE_SAGA_ID), 0);

    assert_eq!(
        ctx.failed_tell_ids(PROBE_LABEL).label,
        PROBE_LABEL,
        "`SagaContext` must not report failed tells: a saga observes failure as \
         a response event from its target plus a deadline it set, not as a \
         transport report the framework hands it"
    );
}

/// Given a blanket extension trait that claims both tell constructors, When
/// each is called on `SagaEffect`, Then both resolve to the extension.
#[test]
fn the_tell_constructor_names_belong_to_downstream_definitions() {
    assert_eq!(
        (
            SagaEffect::<Noted>::tell(PROBE_LABEL).label,
            SagaEffect::<Noted>::tell_intent(PROBE_LABEL).label,
        ),
        (PROBE_LABEL, PROBE_LABEL),
        "`SagaEffect` must offer no way to express a side effect: what a saga \
         persists is a decision, and delivery is a subscription over it"
    );
}

/// Given a saga that also implements a probe extension declaring
/// `on_tell_failed`, When the method is called on the saga, Then it resolves to
/// the probe — the hook is withdrawn with no replacement.
#[test]
fn the_on_tell_failed_name_is_free_for_an_extension_trait() {
    let mut saga = ProbeSaga;

    assert_eq!(
        saga.on_tell_failed(PROBE_LABEL).label,
        PROBE_LABEL,
        "`on_tell_failed` must no longer be a `Saga` trait item competing for \
         this call: transport failure is the delivery host's hold, and nothing \
         replaces the saga-facing notification"
    );
}
