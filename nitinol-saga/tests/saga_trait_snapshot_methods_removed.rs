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

use nitinol_eventsource::Event;
use nitinol_persistence::{EventType, Family, TypeName};
use nitinol_saga::{Saga, SagaContext, SagaEffect};

const PROBE_CAPTURE: &str = "probe-capture";
const PROBE_RESTORE: &str = "probe-restore";

#[derive(Clone)]
struct Probed;

impl Event for Probed {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("saga_trait_snapshot_methods_removed"),
        TypeName::new("Probed"),
    );
}

#[derive(Clone)]
struct Upstream;

impl Event for Upstream {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("saga_trait_snapshot_methods_removed"),
        TypeName::new("Upstream"),
    );
}

#[derive(Default)]
struct ProbedSaga {
    restored_from: &'static str,
}

#[async_trait]
impl Saga for ProbedSaga {
    type SubscribedEvent = Upstream;
    type Event = Probed;
    type ScheduledMessage = ();
    type Error = std::convert::Infallible;

    fn apply(&mut self, _event: Self::Event) {}

    async fn handle(
        &mut self,
        _event: Self::SubscribedEvent,
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
