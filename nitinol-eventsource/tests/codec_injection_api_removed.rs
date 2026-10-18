//! The codec injection API must be gone from this crate: there is no
//! `with_codec` step, no codec typestate to step through, and no type-erased
//! codec handle to pass around.
//!
//! This is the acceptance scenario "the codec injection API does not exist".
//! An API that no
//! longer exists cannot be called, so each half of the contract is observed by
//! letting a downstream definition claim the name or the method, following the
//! idiom `nitinol-saga/tests/saga_snapshot_type_removed.rs` established:
//!
//! - **Names** (`CodecSet`, `CodecUnset`, `Unset`, `ErasedCodec`) — the crate's
//!   namespace and a local `probe` namespace are both glob-imported and the
//!   bare name is used. Once the crate stops exporting it, the name has a
//!   single source and resolves to the probe; while the crate still exports it,
//!   the two globs collide and this test binary fails to build. The probe
//!   definitions carry fields the removed types do not, so resolving the
//!   ambiguity in the crate's favour would not rescue the build either.
//!
//! - **Methods** (`AggregateProps::with_codec`,
//!   `EventSourceSystemBuilder::with_codec`) — a name cannot collide here, so a
//!   downstream extension trait claims the method instead. An inherent method
//!   wins over a trait method, so while the crate still has one the call below
//!   resolves to it and is rejected for its arity or its return type; once the
//!   inherent method is gone the extension trait is the only candidate and the
//!   call means what this test says it means.
//!
//! A definition that is merely hidden — kept in the crate but dropped from the
//! re-export — is invisible here; the crate's own dead-code lint catches that.
//!
//! Run with:
//! `cargo test -p nitinol-eventsource --test codec_injection_api_removed`.

use nitinol_persistence::{AggregateId, EventType, Family, TypeName};
use nitinol_runtime::ProcessSystem;

/// What a downstream crate is free to define once the names are released. Each
/// carries a field the released type never had, so only the probe can be named
/// by the literals below.
mod probe {
    pub struct CodecUnset {
        pub owner: &'static str,
    }

    pub struct CodecSet {
        pub owner: &'static str,
    }

    /// The builder's codec-stage marker, from `nitinol_eventsource::system`.
    pub struct Unset {
        pub owner: &'static str,
    }

    /// The framework-internal type-erased codec handle, from
    /// `nitinol_eventsource::codec`.
    pub struct ErasedCodec {
        pub owner: &'static str,
    }
}

use nitinol_eventsource::codec::*;
use nitinol_eventsource::system::*;
use nitinol_eventsource::*;
use probe::*;

const PROBE_OWNER: &str = "downstream-probe";

// Fixtures: the smallest aggregate that can carry props.

#[derive(Clone)]
struct Bumped;

struct BumpedCodec;

impl Codec<Bumped> for BumpedCodec {
    type Error = std::convert::Infallible;

    fn encode(_event: &Bumped) -> Result<bytes::Bytes, Self::Error> {
        Ok(bytes::Bytes::new())
    }

    fn decode(_payload: &[u8]) -> Result<Bumped, Self::Error> {
        Ok(Bumped)
    }
}

impl Event for Bumped {
    const EVENT_TYPE: EventType = EventType::new(Family::new("probe"), TypeName::new("Bumped"));

    type Codec = BumpedCodec;
}

#[derive(Default)]
struct Counter;

impl Aggregate for Counter {
    type Event = Bumped;
    type Recover = Never;

    fn apply(&mut self, _event: Bumped) {}
}

// The typestate markers

/// Given both namespaces are glob-imported, When the bare `CodecUnset` and
/// `CodecSet` names are used, Then they denote the downstream probe
/// definitions — this crate exports neither any more.
#[test]
fn the_codec_typestate_names_belong_to_downstream_definitions() {
    let unset = CodecUnset { owner: PROBE_OWNER };
    let set = CodecSet { owner: PROBE_OWNER };

    assert_eq!(
        (unset.owner, set.owner),
        (PROBE_OWNER, PROBE_OWNER),
        "`CodecSet` / `CodecUnset` must not be exported by this crate any more"
    );
}

/// Given the builder no longer has a codec stage, When the bare `Unset` name is
/// used, Then it denotes the downstream probe — the marker that stood for "no
/// codec configured yet" is gone with the stage it marked.
#[test]
fn the_builders_codec_stage_marker_belongs_to_downstream_definitions() {
    let marker = Unset { owner: PROBE_OWNER };

    assert_eq!(
        marker.owner, PROBE_OWNER,
        "`system::Unset` must not be exported by this crate any more"
    );
}

/// Given codecs are resolved from the event type rather than stored behind a
/// handle, When the bare `ErasedCodec` name is used, Then it denotes the
/// downstream probe — there is nothing left to erase a codec into.
#[test]
fn the_type_erased_codec_handle_belongs_to_downstream_definitions() {
    let handle = ErasedCodec { owner: PROBE_OWNER };

    assert_eq!(
        handle.owner, PROBE_OWNER,
        "`codec::ErasedCodec` must not be exported by this crate any more"
    );
}

// The injection methods

/// What a downstream crate is free to define once `with_codec` is released.
trait WithCodecProbe {
    fn with_codec(&self) -> &'static str;
}

impl<A: Aggregate> WithCodecProbe for AggregateProps<A> {
    fn with_codec(&self) -> &'static str {
        PROBE_OWNER
    }
}

impl WithCodecProbe for EventSourceSystemBuilder<PersistenceUnset> {
    fn with_codec(&self) -> &'static str {
        PROBE_OWNER
    }
}

/// Given props that were never offered a codec, When `with_codec` is called on
/// them, Then the call reaches the downstream extension trait — the crate has
/// no inherent method of that name to take precedence.
#[test]
fn aggregate_props_has_no_inherent_with_codec() {
    let props = AggregateProps::<Counter>::new(AggregateId::new("probe-props"));

    assert_eq!(
        props.with_codec(),
        PROBE_OWNER,
        "`AggregateProps::with_codec` must not exist any more"
    );
}

/// Given a freshly started builder, When `with_codec` is called on it, Then the
/// call reaches the downstream extension trait — binding a system-wide codec
/// type is no longer a stage of building a system.
#[tokio::test]
async fn the_event_source_system_builder_has_no_inherent_with_codec() {
    let ps = ProcessSystem::new().await;
    let builder = EventSourceSystem::builder(ps);

    assert_eq!(
        builder.with_codec(),
        PROBE_OWNER,
        "`EventSourceSystemBuilder::with_codec` must not exist any more"
    );
}
