//! `Deadline<M>` carries two things and no connection: effect constructors for
//! setting and cancelling, and a read-only view of what is pending.
//!
//! ## What this file pins
//!
//! Three function-pointer coercions, resolved at compile time.  A coercion
//! names the receiver, every parameter and the return type, so it fails if any
//! of them moves — a `set` that returned `Result`, took `self` by value, or
//! answered with a live timer handle instead of a `SagaEffect` would all be
//! caught here.  Nothing is called: `Deadline<M>` has no public constructor,
//! which is the point of `saga_deadline_requires_component.rs`.
//!
//! `set` returning a `SagaEffect` is the whole of "no connection": an effect is
//! interpreted by the framework, which appends the fact and only then arms the
//! timer, so "write no fact and arm a timer anyway" is not a shape the component
//! can express.  A `Deadline` holding a `SchedulerProxy` would have to offer an
//! arming method of its own, and there is no type here it could return.
//!
//! ## What this file cannot pin
//!
//! "And nothing else" — that `Deadline<M>` has no *fourth* public method.
//! Rust cannot enumerate a type's inherent methods, and this workspace has no
//! compile-fail harness (`trybuild`) for integration tests, so an extra method
//! would still let this binary build.  See the test report for the residual
//! check.
//!
//! "No public constructor" is not residual: it is pinned by the `compile_fail`
//! doctests on `Deadline` itself, which is also why nothing here is called.
//! See: nitinol-saga/src/components/deadline.rs
//!
//! ## API contract assumed by this file
//!
//! - `Deadline::set(&self, TimerName, Duration, M) -> SagaEffect<E>`
//! - `Deadline::cancel(&self, TimerName) -> SagaEffect<E>`
//! - `Deadline::pending(&self) -> Vec<PendingDeadline>`
//! - `DeadlineMessage { type Codec: Codec<Self> }`
//!
//! Run with: `cargo test -p nitinol-saga --test saga_deadline_surface`.

use std::time::Duration;

use bytes::Bytes;

use nitinol_eventsource::codec::Codec;
use nitinol_eventsource::Event;
use nitinol_persistence::{EventType, Family, TypeName};
use nitinol_saga::{Deadline, DeadlineMessage, PendingDeadline, SagaEffect, TimerName};

/// The saga's own event type the effects below are parameterised by.  Nothing
/// in this file writes to a store, but an effect is an effect over *some*
/// event type, so one is declared.
#[derive(Clone)]
struct Recorded;

struct UnitCodec;

impl Codec<Recorded> for UnitCodec {
    type Error = std::convert::Infallible;

    fn encode(_value: &Recorded) -> Result<Bytes, Self::Error> {
        Ok(Bytes::new())
    }

    fn decode(_payload: &[u8]) -> Result<Recorded, Self::Error> {
        Ok(Recorded)
    }
}

impl Event for Recorded {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("deadline_surface"), TypeName::new("Recorded"));
    type Codec = UnitCodec;
}

/// The deadline message.  Its format is its own declaration, which is what
/// makes it eligible to be persisted inside a schedule fact.
#[derive(Clone)]
struct Reminder;

impl Codec<Reminder> for UnitCodec {
    type Error = std::convert::Infallible;

    fn encode(_value: &Reminder) -> Result<Bytes, Self::Error> {
        Ok(Bytes::new())
    }

    fn decode(_payload: &[u8]) -> Result<Reminder, Self::Error> {
        Ok(Reminder)
    }
}

impl DeadlineMessage for Reminder {
    const TYPE_NAME: &'static str = "reminder";
    const CODEC_NAME: &'static str = "unit-codec";
    type Codec = UnitCodec;
}

/// Setting a deadline produces an effect — the framework's to interpret — so
/// the fact is written before any timer exists.
const SET_IS_AN_EFFECT_CONSTRUCTOR: fn(
    &Deadline<Reminder>,
    TimerName,
    Duration,
    Reminder,
) -> SagaEffect<Recorded> = Deadline::<Reminder>::set;

/// Cancelling is the same kind of thing: an effect, not a call to a timer.
const CANCEL_IS_AN_EFFECT_CONSTRUCTOR: fn(&Deadline<Reminder>, TimerName) -> SagaEffect<Recorded> =
    Deadline::<Reminder>::cancel;

/// Reading is read-only: `&self`, and an owned snapshot out.
const PENDING_IS_A_READ_ONLY_VIEW: fn(&Deadline<Reminder>) -> Vec<PendingDeadline> =
    Deadline::<Reminder>::pending;

/// Given the three coercions above resolved, Then the component's surface is
/// the effect pair plus the view.
///
/// The assertions are the coercions; this test exists so the file reports as a
/// test rather than as a silent compile unit, and so the three constants are
/// used rather than dead.
#[test]
fn the_deadline_component_is_two_effect_constructors_and_one_view() {
    let pointers: [usize; 3] = [
        SET_IS_AN_EFFECT_CONSTRUCTOR as usize,
        CANCEL_IS_AN_EFFECT_CONSTRUCTOR as usize,
        PENDING_IS_A_READ_ONLY_VIEW as usize,
    ];

    assert!(
        pointers.iter().all(|address| *address != 0),
        "the three operations `Deadline<M>` offers must resolve with the \
         signatures this file coerces them to"
    );
}
