//! Acceptance scenario "the framework carries no envelope": documenting tell
//! dispatch and the idempotent-receive pattern must not give the framework a
//! metadata / causation envelope, so a domain that needs an operation id carries
//! it as its own data.
//!
//! The three types a dispatched command's facts travel through — the record a
//! write appends, the record a read returns, and the decision a command produced
//! — must therefore still carry exactly the fields they carried before.
//!
//! The *absence* of a field is not observable at runtime, so the contract is
//! carried by this file building at all: every value below is constructed field
//! by field and then taken apart field by field, with no `..`, no
//! `..Default::default()` and no wildcard anywhere.  A metadata field added to
//! any of the three makes a construction incomplete (E0063) or a pattern
//! incomplete (E0027 / E0023), and this test binary stops building — which is
//! equally true if the field is merely hidden behind a `#[doc(hidden)]` or a
//! deprecation attribute instead of being absent.
//!
//! The types are checked through the umbrella crate because the claim spans two
//! of them: `AppendingEvent` and `LoadedEvent` belong to `nitinol-persistence`
//! and `Decision` to `nitinol-contract`, and the `contract` feature reaches
//! both.
//!
//! Run with:
//! `cargo test -p nitinol --features contract --test dispatch_carries_no_envelope`.
#![cfg(feature = "contract")]

use bytes::Bytes;
use jiff::Timestamp;

use nitinol::contract::Decision;
use nitinol::persistence::{AppendingEvent, EventType, Family, LoadedEvent, TypeName};

/// The identity the fixture records below are written under.
///
/// Nothing here reaches a store — the records are built and read back in
/// memory — but both structs name an `EventType`, so one is declared.
const FIXTURE_EVENT_TYPE: EventType = EventType::new(
    Family::new("dispatch_carries_no_envelope"),
    TypeName::new("Fixture"),
);

/// The payload the fixture records carry.  Its content is irrelevant; what
/// matters is that `payload` is the only place a record carries opaque bytes.
const FIXTURE_PAYLOAD: &[u8] = b"fixture";

/// Given the record an append is handed, When every field it carries is named,
/// Then the four a write needs are the whole of it — no envelope travels
/// alongside the payload.
#[test]
fn an_appending_event_carries_only_what_a_write_needs() {
    let occurred_at = Timestamp::now();

    let event = AppendingEvent {
        sequence: 1,
        event_type: FIXTURE_EVENT_TYPE,
        payload: Bytes::from_static(FIXTURE_PAYLOAD),
        occurred_at,
    };

    let AppendingEvent {
        sequence,
        event_type,
        payload,
        occurred_at: stamped,
    } = event;

    assert_eq!(
        sequence, 1,
        "an appending event carries the stream sequence it claims"
    );
    assert_eq!(
        event_type, FIXTURE_EVENT_TYPE,
        "an appending event carries the identity its rows are written under"
    );
    assert_eq!(
        payload,
        Bytes::from_static(FIXTURE_PAYLOAD),
        "the domain's own bytes are the only content a record carries; an operation \
         id belongs inside them, as domain data, not beside them"
    );
    assert_eq!(
        stamped, occurred_at,
        "an appending event carries the timestamp it was handed"
    );
}

/// Given the record a load returns, When every field it carries is named, Then
/// the six a read returns are the whole of it.
#[test]
fn a_loaded_event_carries_only_what_a_read_returns() {
    let occurred_at = Timestamp::now();

    let event = LoadedEvent {
        stream_key: "dispatch-carries-no-envelope".to_owned(),
        sequence: 1,
        global_sequence: 1,
        event_type: FIXTURE_EVENT_TYPE,
        payload: Bytes::from_static(FIXTURE_PAYLOAD),
        occurred_at,
    };

    let LoadedEvent {
        stream_key,
        sequence,
        global_sequence,
        event_type,
        payload,
        occurred_at: stamped,
    } = event;

    assert_eq!(
        stream_key, "dispatch-carries-no-envelope",
        "a loaded event carries the stream key it was appended under"
    );
    assert_eq!(sequence, 1, "a loaded event carries its stream sequence");
    assert_eq!(
        global_sequence, 1,
        "a loaded event carries the store-wide sequence assigned at commit"
    );
    assert_eq!(
        event_type, FIXTURE_EVENT_TYPE,
        "a loaded event carries the identity it was written under"
    );
    assert_eq!(
        payload,
        Bytes::from_static(FIXTURE_PAYLOAD),
        "the domain's own bytes are the only content a read returns; a receiver that \
         needs an operation id reads it out of them"
    );
    assert_eq!(
        stamped, occurred_at,
        "a loaded event carries the timestamp of the append that committed it"
    );
}

/// Given a decider's conclusion, When both of its cases are taken apart, Then an
/// acceptance is facts plus an answer and a refusal is the rejection alone.
///
/// The empty acceptance is the value exhausted here because it is the shape the
/// idempotent-receive pattern answers a duplicate with (L-3): a duplicate is
/// recognised from the receiver's own state, not from an envelope the framework
/// attached.
#[test]
fn a_decision_carries_only_facts_and_an_answer_or_a_refusal() {
    let accepted: Decision<&str, u64, &str> = Decision::persist(Vec::new()).output(7);

    match accepted {
        Decision::Accept { events, output } => {
            assert!(
                events.is_empty(),
                "an acceptance that states no facts appends nothing"
            );
            assert_eq!(
                output, 7,
                "an acceptance that states no facts still answers the caller"
            );
        }
        Decision::Reject(rejection) => {
            panic!("an empty acceptance must not be a refusal (got {rejection:?})")
        }
    }

    match Decision::<&str, u64, &str>::reject("refused") {
        Decision::Accept { events, output } => panic!(
            "a refusal must not be an acceptance (got events {events:?} and output {output:?})"
        ),
        Decision::Reject(rejection) => assert_eq!(
            rejection, "refused",
            "a refusal carries the rejection and nothing else"
        ),
    }
}
