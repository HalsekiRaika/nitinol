//! An event and a snapshot name the codec that writes them down, and they name
//! it here — in the contract, where the type is defined — rather than at the
//! wiring site that later activates them.
//!
//! What this file pins is the *binding*, not any one codec:
//!
//! - `Codec` is reachable from `nitinol-contract` (it is either defined here or
//!   re-exported from a layer this crate already depends on), so a domain crate
//!   can write `type Codec = ..` without reaching for an interpreter.
//! - `Event` itself guarantees `Self::Codec: Codec<Self>`, and `Snapshotable`
//!   itself guarantees `Self::Codec: Codec<Self::Snapshot>`. The two generic
//!   functions below settle that: each calls a helper whose only bound is the
//!   one the trait is supposed to supply, so they compile if and only if the
//!   trait declares it. An associated type left unbounded — or bounded against
//!   the wrong subject — fails to compile here while a per-type `where` clause
//!   at the call site would have hidden the difference.
//!
//! No async runtime and no `nitinol-eventsource` here: naming a codec is part
//! of defining a domain type, so it must be expressible with the contract crate
//! alone, exactly as `decider_query.rs` requires of a decision.
//!
//! Run with: `cargo test -p nitinol-contract --test event_and_snapshot_name_their_codec`.

use std::convert::Infallible;
use std::fmt;

use bytes::Bytes;
use nitinol_contract::{Aggregate, Codec, Event, Snapshotable};
use nitinol_persistence::{EventType, Family, TypeName};

// Fixture: an event and the codec it names.

#[derive(Clone, Debug, PartialEq, Eq)]
struct Credited {
    amount: u64,
}

/// A deliberately unmistakable wire format: the tag is what lets a later test
/// tell "the codec this event named" apart from "some codec the wiring had
/// lying around".
struct CreditedCodec;

impl CreditedCodec {
    const TAG: &'static [u8] = b"credited:";
}

#[derive(Debug, PartialEq, Eq)]
struct MalformedCredited;

impl fmt::Display for MalformedCredited {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("payload is not a `credited:` record")
    }
}

impl std::error::Error for MalformedCredited {}

impl Codec<Credited> for CreditedCodec {
    type Error = MalformedCredited;

    fn encode(event: &Credited) -> Result<Bytes, Self::Error> {
        let mut out = Vec::from(Self::TAG);
        out.extend_from_slice(&event.amount.to_be_bytes());
        Ok(Bytes::from(out))
    }

    fn decode(payload: &[u8]) -> Result<Credited, Self::Error> {
        let body = payload.strip_prefix(Self::TAG).ok_or(MalformedCredited)?;
        let amount = body
            .try_into()
            .map(u64::from_be_bytes)
            .map_err(|_| MalformedCredited)?;
        Ok(Credited { amount })
    }
}

// Written by hand rather than derived: `nitinol-contract` does not depend on
// `nitinol-macros`, and the derive is pinned in that crate's own suite.
impl Event for Credited {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("shop.wallet"), TypeName::new("Credited"));

    type Codec = CreditedCodec;
}

// Fixture: an aggregate and the codec its snapshot names.

#[derive(Debug, Default, PartialEq, Eq)]
struct Wallet {
    balance: u64,
}

impl Aggregate for Wallet {
    type Event = Credited;

    fn apply(&mut self, event: Credited) {
        self.balance += event.amount;
    }
}

/// A different format from the event's, so that the two associated types cannot
/// both be satisfied by one accidental blanket impl.
struct WalletSnapshotCodec;

impl Codec<u64> for WalletSnapshotCodec {
    type Error = Infallible;

    fn encode(value: &u64) -> Result<Bytes, Self::Error> {
        Ok(Bytes::from(value.to_le_bytes().to_vec()))
    }

    fn decode(payload: &[u8]) -> Result<u64, Self::Error> {
        let mut buf = [0u8; 8];
        let taken = payload.len().min(8);
        buf[..taken].copy_from_slice(&payload[..taken]);
        Ok(u64::from_le_bytes(buf))
    }
}

impl Snapshotable for Wallet {
    type Snapshot = u64;
    type Codec = WalletSnapshotCodec;

    fn capture(&self) -> u64 {
        self.balance
    }

    fn restore(snapshot: u64) -> Self {
        Self { balance: snapshot }
    }
}

// The binding, observed generically.

/// Carries no bound of its own beyond the one under test, so it can only be
/// called if the caller already has it.
fn demands_a_codec_for<C: Codec<T>, T>() {}

/// Given nothing but `E: Event`, When `E::Codec` is required to be `Codec<E>`,
/// Then the requirement is already discharged — the `Event` trait supplies it.
fn event_codec_is_bound_by_the_trait<E: Event>() {
    demands_a_codec_for::<E::Codec, E>();
}

/// Given nothing but `A: Snapshotable`, When `A::Codec` is required to be
/// `Codec<A::Snapshot>`, Then the `Snapshotable` trait supplies it.
fn snapshot_codec_is_bound_by_the_trait<A: Snapshotable>() {
    demands_a_codec_for::<A::Codec, A::Snapshot>();
}

/// Given the two generic demands above, When they are instantiated at the
/// fixture types, Then both resolve — the traits carry the bounds, so no
/// interpreter has to restate them.
#[test]
fn the_contract_binds_each_codec_to_what_it_writes() {
    event_codec_is_bound_by_the_trait::<Credited>();
    snapshot_codec_is_bound_by_the_trait::<Wallet>();
}

// The binding, observed by value.

/// Given an event reached only through `<Credited as Event>::Codec`, When it is
/// encoded and decoded, Then the value survives and the bytes carry the named
/// codec's tag — a codec chosen anywhere other than the event definition would
/// not produce them.
#[test]
fn an_event_round_trips_through_the_codec_it_names() {
    let event = Credited { amount: 1234 };

    let payload = <<Credited as Event>::Codec as Codec<Credited>>::encode(&event)
        .expect("the event must encode through the codec it names");

    assert!(
        payload.starts_with(CreditedCodec::TAG),
        "the bytes must come from `CreditedCodec`, the codec `Credited` names"
    );

    let decoded = <<Credited as Event>::Codec as Codec<Credited>>::decode(&payload)
        .expect("the event must decode through the codec it names");

    assert_eq!(decoded, event);
}

/// Given a payload the event's codec did not write, When it is decoded through
/// `<Credited as Event>::Codec`, Then it is refused — the association is to one
/// concrete format, not to "whatever bytes happen to be in the stream".
#[test]
fn a_foreign_payload_is_refused_by_the_codec_the_event_names() {
    let refusal = <<Credited as Event>::Codec as Codec<Credited>>::decode(b"{\"amount\":1234}")
        .expect_err("a payload in another format must not decode");

    assert_eq!(refusal, MalformedCredited);
}

/// Given a snapshot captured from an aggregate, When it is written and read
/// back through `<Wallet as Snapshotable>::Codec`, Then the aggregate restores
/// to the state it captured.
#[test]
fn a_snapshot_round_trips_through_the_codec_it_names() {
    let mut wallet = Wallet::default();
    wallet.apply(Credited { amount: 40 });
    wallet.apply(Credited { amount: 2 });

    let captured = wallet.capture();
    let payload = <<Wallet as Snapshotable>::Codec as Codec<u64>>::encode(&captured)
        .expect("the snapshot must encode through the codec it names");
    let decoded = <<Wallet as Snapshotable>::Codec as Codec<u64>>::decode(&payload)
        .expect("the snapshot must decode through the codec it names");

    assert_eq!(Wallet::restore(decoded), Wallet { balance: 42 });
}

/// Given the event codec and the snapshot codec are named independently, When
/// the snapshot payload is offered to the event's codec, Then it is refused —
/// two associated types, two formats, no shared fallback.
#[test]
fn the_two_named_codecs_are_not_interchangeable() {
    let snapshot = <<Wallet as Snapshotable>::Codec as Codec<u64>>::encode(&42)
        .expect("the snapshot must encode");

    let refusal = <<Credited as Event>::Codec as Codec<Credited>>::decode(&snapshot)
        .expect_err("the event codec must not accept snapshot bytes");

    assert_eq!(refusal, MalformedCredited);
}
