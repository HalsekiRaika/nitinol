//! `Saga` must carry no `ScheduledMessage` associated type and no
//! `on_scheduled` entry point.
//!
//! A saga's own time is now a `#[subscribe(deadline)]` variant of its
//! alphabet, so a timer firing arrives at the one reactive entry point every
//! other input arrives at.  A surviving second entry point would be a place for
//! a decision to be made that `handle`'s `match` does not account for, and a
//! surviving `ScheduledMessage` would be a required type ceremony for every
//! saga that never sets a deadline.
//!
//! ## How the absence is observed
//!
//! The absence of a trait item is not observable at runtime, so the contract is
//! carried by this file building at all: `MinimalSaga` declares every
//! associated type the trait still *requires* and no `ScheduledMessage`.  While
//! `Saga::ScheduledMessage` exists the impl is incomplete (E0046) and this test
//! binary fails to build — which is equally true of an item merely "disabled"
//! by a deprecation attribute or a comment instead of being deleted.
//!
//! `on_scheduled` is settled by the same fact rather than by an assertion of
//! its own: its signature is stated in `Self::ScheduledMessage`, so it cannot
//! outlive the type it is written in terms of.  Where a timer firing *does*
//! arrive is pinned positively in `saga_deadline_scheduler.rs`.  A defaulted
//! hook of some new shape would still let this binary build; see the test
//! report for the residual check.
//!
//! ## API contract assumed by this file
//!
//! - `Saga` requires exactly `type Subscribe`, `type Event`, `type Error`,
//!   `fn correlate`, and `async fn handle`.  `apply` and `on_tell_failed` carry
//!   defaults, which is why neither appears below.
//!
//! Run with:
//! `cargo test -p nitinol-saga --test saga_scheduled_message_api_removed`.

use async_trait::async_trait;

use bytes::Bytes;

use nitinol::saga::SagaSubscribe;
use nitinol_eventsource::codec::Codec;
use nitinol_eventsource::Event;
use nitinol_persistence::{EventType, Family, TypeName};
use nitinol_saga::{Saga, SagaContext, SagaEffect, SagaId};

/// The format the events below name.  Nothing in this file writes to a store —
/// `correlate` is called directly — but `Event` names the codec that would
/// write it, so one is declared.
struct UnitCodec;

impl Codec<Signal> for UnitCodec {
    type Error = std::convert::Infallible;

    fn encode(value: &Signal) -> Result<Bytes, Self::Error> {
        Ok(Bytes::from(value.owner.clone().into_bytes()))
    }

    fn decode(payload: &[u8]) -> Result<Signal, Self::Error> {
        Ok(Signal {
            owner: String::from_utf8_lossy(payload).into_owned(),
        })
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

/// The one correlation rule of [`MinimalSaga`].
const OWNER: &str = "scheduled-message-removed-owner";

#[derive(Clone)]
struct Signal {
    owner: String,
}

impl Event for Signal {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("saga_scheduled_message_removed"),
        TypeName::new("Signal"),
    );
    type Codec = UnitCodec;
}

#[derive(Clone)]
struct Noted;

impl Event for Noted {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("saga_scheduled_message_removed"),
        TypeName::new("Noted"),
    );
    type Codec = UnitCodec;
}

/// One external source and no deadline: a saga that never sets one names
/// nothing extra.
#[derive(SagaSubscribe)]
enum Observed {
    Signal(Signal),
}

/// Implements exactly the trait items the `Saga` trait still requires, and
/// nothing else.
struct MinimalSaga;

#[async_trait]
impl Saga for MinimalSaga {
    type Subscribe = Observed;
    type Event = Noted;
    type Error = std::convert::Infallible;

    fn correlate(input: &Self::Subscribe) -> Option<SagaId> {
        let Observed::Signal(signal) = input;
        (signal.owner == OWNER).then(|| SagaId::new(OWNER))
    }

    async fn handle(
        &mut self,
        _input: Self::Subscribe,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        Ok(SagaEffect::None)
    }
}

/// Given a saga declared without a `ScheduledMessage` type and without an
/// `on_scheduled` hook, When its one required associated function is called,
/// Then the saga is a complete implementation of the trait.
///
/// The call is incidental; the assertion the file carries is that it compiles.
#[test]
fn a_saga_that_names_no_scheduled_message_type_is_complete() {
    let input = Observed::Signal(Signal {
        owner: OWNER.to_owned(),
    });

    assert_eq!(
        <MinimalSaga as Saga>::correlate(&input),
        Some(SagaId::new(OWNER)),
        "a saga that sets no deadline must satisfy the trait with its alphabet, \
         its own event type and its error type alone"
    );
}
