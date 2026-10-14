//! Attributing an undecodable upstream record is no longer a setting.
//!
//! `with_decode_failure_route` existed to decide which instance's dead-letter
//! queue a corrupt wire record belonged to.  With no dead-letter destination for
//! it to name, the question it answered is gone: a record whose bytes cannot be
//! decoded is a deterministic, permanent failure with no typed event to
//! correlate on, so the one thing to do with it is log it and move the cursor
//! past it.  That behaviour is pinned positively in
//! `saga_upstream_decode_failure_is_skipped.rs`; what is pinned here is that the
//! builder no longer takes a route function.
//!
//! ## How the absence is observed
//!
//! A blanket extension trait claims the `with_decode_failure_route` name.  An
//! inherent method wins method resolution over a trait one, so while
//! `SagaManagerProps::with_decode_failure_route` exists the call below is
//! checked against *it* and fails on its argument; once it is gone the blanket
//! method is the only candidate and resolves.  `SagaManagerProps` is the one
//! saga builder there is to probe: a saga whose `correlate` always answers the
//! same id is the degenerate case of the manager, so the route function is
//! withdrawn from the only path that could still have decided attribution.
//!
//! `DecodeFailureRouteFn` is `pub(crate)`, so its removal is not observable from
//! a downstream build at all; a definition left behind with no caller is caught
//! by the crate's own dead-code lint (`cargo clippy --all-targets -- -D
//! warnings`) rather than here.  See the test report.
//!
//! Run with:
//! `cargo test -p nitinol-saga --test saga_decode_failure_route_api_removed`.

use async_trait::async_trait;
use bytes::Bytes;

use nitinol::saga::SagaSubscribe;
use nitinol_eventsource::codec::Codec;
use nitinol_eventsource::Event;
use nitinol_persistence::store::{InMemoryEventStore, Journal};
use nitinol_persistence::{EventType, Family, TypeName};
use nitinol_saga::{Saga, SagaContext, SagaEffect, SagaId, SagaManagerProps};

/// What `with_decode_failure_route` means to a downstream crate once the
/// builder stops claiming the name: nothing the framework decides.
mod probe {
    pub struct Declined {
        pub label: &'static str,
    }

    pub trait DecodeFailuresAreSkipped {
        fn with_decode_failure_route(&self, label: &'static str) -> Declined {
            Declined { label }
        }
    }

    impl<T> DecodeFailuresAreSkipped for T {}
}

use probe::DecodeFailuresAreSkipped;

const PROBE_LABEL: &str = "downstream-label";

// A minimal saga, so a builder value exists to call the probed method on.

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
        Family::new("saga_decode_failure_route_removed"),
        TypeName::new("Signal"),
    );
    type Codec = UnitCodec;
}

#[derive(Clone)]
struct Noted;

impl Event for Noted {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("saga_decode_failure_route_removed"),
        TypeName::new("Noted"),
    );
    type Codec = UnitCodec;
}

#[derive(SagaSubscribe)]
enum Observed {
    Signal(Signal),
}

struct ProbeSaga;

#[async_trait]
impl Saga for ProbeSaga {
    type Subscribe = Observed;
    type Event = Noted;
    type Error = std::convert::Infallible;

    fn correlate(_input: &Self::Subscribe) -> Option<SagaId> {
        Some(SagaId::new("decode-failure-route-removed"))
    }

    async fn handle(
        &mut self,
        _input: Self::Subscribe,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        Ok(SagaEffect::None)
    }
}

fn store() -> Journal {
    Journal::from(InMemoryEventStore::default())
}

/// Given a blanket extension trait that claims the
/// `with_decode_failure_route` name,
/// When it is called on the saga builder,
/// Then it resolves to the extension — the builder has no inherent method of
/// that name to win resolution.
#[test]
fn the_decode_failure_route_name_belongs_to_downstream_definitions() {
    let manager_props = SagaManagerProps::<ProbeSaga>::new(store(), |_components| ProbeSaga);

    assert_eq!(
        manager_props.with_decode_failure_route(PROBE_LABEL).label,
        PROBE_LABEL,
        "`SagaManagerProps` must not take a decode-failure route: a record that \
         cannot be decoded has no owner to attribute it to and no dead letter \
         to attribute it into"
    );
}
