//! `Saga` must expose neither `snapshot` nor `from_snapshot`.
//!
//! A method that no longer exists cannot be called, so each contract is
//! observed through a probe extension trait that claims the same name for the
//! same receiver:
//!
//! - `SnapshotProbe::snapshot` and `FromSnapshotProbe::from_snapshot` are the
//!   only candidates once the trait items are gone, so the calls below resolve
//!   to the probes and return the probe's own values.
//! - While either `Saga` item survives — including a survivor whose panicking
//!   body was merely swapped for another panic — the matching call has two
//!   applicable candidates and this test binary fails to build (E0034).
//!
//! The receiver has to implement `Saga` for the trait's items to be candidates
//! at all, so `ProbedSaga` is a full implementation of the remaining surface.

use async_trait::async_trait;

use bytes::Bytes;
use nitinol_eventsource::codec::Codec;
use nitinol_eventsource::Event;
use nitinol_persistence::{EventType, Family, TypeName};
use nitinol_saga::{Saga, SagaContext, SagaEffect, SagaId, SagaSubscribe};

/// The format the events below name.  Nothing in this file writes to a store —
/// the probes are called directly — but `Event` names the codec that would
/// write it, so one is declared.
struct UnitCodec;

impl Codec<Probed> for UnitCodec {
    type Error = std::convert::Infallible;

    fn encode(_event: &Probed) -> Result<Bytes, Self::Error> {
        Ok(Bytes::new())
    }

    fn decode(_payload: &[u8]) -> Result<Probed, Self::Error> {
        Ok(Probed)
    }
}

impl Codec<Upstream> for UnitCodec {
    type Error = std::convert::Infallible;

    fn encode(_event: &Upstream) -> Result<Bytes, Self::Error> {
        Ok(Bytes::new())
    }

    fn decode(_payload: &[u8]) -> Result<Upstream, Self::Error> {
        Ok(Upstream)
    }
}

const PROBE_CAPTURE: &str = "probe-capture";
const PROBE_RESTORE: &str = "probe-restore";

#[derive(Clone)]
struct Probed;

impl Event for Probed {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("saga_trait_snapshot_methods_removed"),
        TypeName::new("Probed"),
    );
    type Codec = UnitCodec;
}

#[derive(Clone)]
struct Upstream;

impl Event for Upstream {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("saga_trait_snapshot_methods_removed"),
        TypeName::new("Upstream"),
    );
    type Codec = UnitCodec;
}

/// Correlation rule of [`ProbedSaga`]: the single process instance every
/// upstream event in this file belongs to.
const PROBED_SAGA_ID: &str = "saga-trait-snapshot-methods-removed";

/// The whole alphabet this saga reacts to: one observed source, no deadline.
#[derive(SagaSubscribe)]
enum Observed {
    Upstream(Upstream),
}

#[derive(Default)]
struct ProbedSaga {
    restored_from: &'static str,
}

#[async_trait]
impl Saga for ProbedSaga {
    type Subscribe = Observed;
    type Event = Probed;
    type Error = std::convert::Infallible;

    fn correlate(_input: &Self::Subscribe) -> Option<SagaId> {
        Some(SagaId::new(PROBED_SAGA_ID))
    }

    async fn handle(
        &mut self,
        _input: Self::Subscribe,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        Ok(SagaEffect::None)
    }
}

/// Claims the `snapshot` name for a saga receiver.  A downstream extension
/// trait may only do this once `Saga` has stopped claiming it.
trait SnapshotProbe {
    fn snapshot(&self) -> &'static str;
}

impl SnapshotProbe for ProbedSaga {
    fn snapshot(&self) -> &'static str {
        PROBE_CAPTURE
    }
}

/// Claims the `from_snapshot` name for a saga receiver, mirroring the
/// restore half of the pair.
trait FromSnapshotProbe {
    fn from_snapshot(marker: &'static str) -> Self;
}

impl FromSnapshotProbe for ProbedSaga {
    fn from_snapshot(marker: &'static str) -> Self {
        Self {
            restored_from: marker,
        }
    }
}

/// Given a saga that also implements a probe extension declaring `snapshot`,
/// when the method is called on the saga, then it resolves to the probe.
#[test]
fn snapshot_name_is_free_for_an_extension_trait() {
    let saga = ProbedSaga::default();

    assert_eq!(
        saga.snapshot(),
        PROBE_CAPTURE,
        "`snapshot` must no longer be a Saga trait item competing for this call"
    );
}

/// Given a saga that also implements a probe extension declaring
/// `from_snapshot`, when the associated function is called, then it resolves
/// to the probe and returns the probe's value instead of panicking.
#[test]
fn from_snapshot_name_is_free_for_an_extension_trait() {
    let saga = ProbedSaga::from_snapshot(PROBE_RESTORE);

    assert_eq!(
        saga.restored_from, PROBE_RESTORE,
        "`from_snapshot` must no longer be a Saga trait item competing for this call"
    );
}
