//! Behavioral equivalence tests for `#[derive(Event)]`.
//!
//! The derive is pure sugar: its expansion must be
//! *functionally equivalent* to the hand-written `impl Event` pattern pinned in
//! `nitinol-eventsource/tests/event_variant.rs`. The hand-written naming rule is
//! "type name = type identifier, variant = arm identifier"; these tests assert
//! the derived `EVENT_TYPE` const and per-arm `variant()` values reproduce that
//! rule exactly.
//!
//! Both the `Event` trait and the `Event` derive are reached through the single
//! umbrella path `nitinol::eventsource::Event` (trait namespace vs. macro
//! namespace). This is the compatibility re-export path; the generated code
//! itself references the canonical `::nitinol::contract::Event` and
//! `::nitinol::persistence::{...}`.
//!
//! `Event` also names the codec that writes the event down, so equivalence now
//! covers a second generated item: `#[event(codec = X)]` must produce
//! `type Codec = X` and nothing else. Each fixture below names a codec of its
//! own, and `codec_is_the_type_the_attribute_named` reads the association back
//! at the type level — a derive that substituted, defaulted or inferred a codec
//! would fail there rather than silently write the wrong format.

use bytes::Bytes;
use nitinol::contract::Codec;
use nitinol::eventsource::Event;
use nitinol::persistence::{EventType, Family, TypeName, Variant};

/// Passes only if `E`'s associated codec *is* `C`, not merely some type that
/// could encode `E`.
fn codec_of<E, C>()
where
    E: Event<Codec = C>,
{
}

// struct: type-level identity (variant None), default variant() inherited

#[derive(Clone, Event)]
#[event(family = "shop.orders", codec = IncrementedCodec)]
struct Incremented;

struct IncrementedCodec;

impl Codec<Incremented> for IncrementedCodec {
    type Error = std::convert::Infallible;

    fn encode(_event: &Incremented) -> Result<Bytes, Self::Error> {
        Ok(Bytes::new())
    }

    fn decode(_payload: &[u8]) -> Result<Incremented, Self::Error> {
        Ok(Incremented)
    }
}

/// Given a struct with `#[derive(Event)]` and an explicit family, When its
/// `EVENT_TYPE` is read, Then it equals the hand-written
/// `EventType::new(Family::new(family), TypeName::new(<type ident>))`.
#[test]
fn struct_event_type_matches_handwritten_const() {
    let expected = EventType::new(Family::new("shop.orders"), TypeName::new("Incremented"));

    assert_eq!(Incremented::EVENT_TYPE, expected);
    assert_eq!(Incremented::EVENT_TYPE.family(), Family::new("shop.orders"));
    assert_eq!(
        Incremented::EVENT_TYPE.type_name(),
        TypeName::new("Incremented")
    );
    assert_eq!(Incremented::EVENT_TYPE.variant(), None);
}

/// Given a struct event (no per-arm override), When `variant()` is called on a
/// value, Then it returns the type-level `EVENT_TYPE` (variant `None`),
/// matching the trait's default `variant()`.
#[test]
fn struct_value_variant_is_type_level_identity() {
    let event = Incremented;

    assert_eq!(event.variant(), Incremented::EVENT_TYPE);
    assert_eq!(event.variant().variant(), None);
}

// struct: empty family, mirroring `Family::new("")` in event_variant.rs

#[derive(Clone, Event)]
#[event(family = "", codec = EmptyFamilyCodec)]
struct EmptyFamily;

struct EmptyFamilyCodec;

impl Codec<EmptyFamily> for EmptyFamilyCodec {
    type Error = std::convert::Infallible;

    fn encode(_event: &EmptyFamily) -> Result<Bytes, Self::Error> {
        Ok(Bytes::new())
    }

    fn decode(_payload: &[u8]) -> Result<EmptyFamily, Self::Error> {
        Ok(EmptyFamily)
    }
}

/// Given a struct with an explicitly empty family, When `EVENT_TYPE` is read,
/// Then the family is the empty `Family` (not a fallback or default) and the
/// rendered form omits the family prefix.
#[test]
fn struct_empty_family_is_preserved_verbatim() {
    assert_eq!(EmptyFamily::EVENT_TYPE.family(), Family::new(""));
    assert_eq!(
        EmptyFamily::EVENT_TYPE.type_name(),
        TypeName::new("EmptyFamily")
    );
    assert_eq!(EmptyFamily::EVENT_TYPE.variant(), None);

    // Display omits the empty family prefix (persistence contract), proving the
    // derived const carries a genuinely empty family.
    assert_eq!(format!("{}", EmptyFamily::EVENT_TYPE), "EmptyFamily");
}

// enum: const is type-level (variant None); variant() fills the active arm

#[derive(Clone, Event)]
#[event(family = "saga.upstream", codec = OrderEventCodec)]
enum OrderEvent {
    Placed,
    Cancelled,
}

struct OrderEventCodec;

impl Codec<OrderEvent> for OrderEventCodec {
    type Error = std::convert::Infallible;

    fn encode(_event: &OrderEvent) -> Result<Bytes, Self::Error> {
        Ok(Bytes::new())
    }

    fn decode(_payload: &[u8]) -> Result<OrderEvent, Self::Error> {
        Ok(OrderEvent::Placed)
    }
}

/// Given an enum with `#[derive(Event)]`, When its `EVENT_TYPE` const is read,
/// Then it is the type-level identity (variant `None`) named after the enum.
#[test]
fn enum_const_event_type_is_type_level() {
    let expected = EventType::new(Family::new("saga.upstream"), TypeName::new("OrderEvent"));

    assert_eq!(OrderEvent::EVENT_TYPE, expected);
    assert_eq!(OrderEvent::EVENT_TYPE.variant(), None);
}

/// Given an enum with `#[derive(Event)]`, When `variant()` is called on each
/// arm, Then it returns the arm-specific EventType whose variant equals the arm
/// identifier — identical to the hand-written match in event_variant.rs.
#[test]
fn enum_variant_returns_arm_identifier() {
    assert_eq!(
        OrderEvent::Placed.variant(),
        EventType::with_variant(
            Family::new("saga.upstream"),
            TypeName::new("OrderEvent"),
            Variant::new("Placed"),
        ),
    );
    assert_eq!(
        OrderEvent::Cancelled.variant(),
        EventType::with_variant(
            Family::new("saga.upstream"),
            TypeName::new("OrderEvent"),
            Variant::new("Cancelled"),
        ),
    );

    assert_eq!(
        OrderEvent::Placed.variant().variant(),
        Some(Variant::new("Placed"))
    );
    assert_eq!(
        OrderEvent::Cancelled.variant().variant(),
        Some(Variant::new("Cancelled"))
    );
}

/// Given an enum event whose arms carry `Some(variant)` identities, When
/// `type_key()` is taken, Then every arm shares the variant-free key of the
/// type-level `EVENT_TYPE` (so value-level events route to the type handler).
#[test]
fn enum_arms_share_type_key_with_const() {
    assert_eq!(
        OrderEvent::Placed.variant().type_key(),
        OrderEvent::EVENT_TYPE.type_key()
    );
    assert_eq!(
        OrderEvent::Cancelled.variant().type_key(),
        OrderEvent::EVENT_TYPE.type_key()
    );
}

// enum: unit / tuple / struct arms all match; arm fields are ignored

// Arm payloads exist only so the derive has tuple/struct shapes to expand over;
// the tests read `variant()`, never the field values.
#[allow(dead_code)]
#[derive(Clone, Event)]
#[event(family = "domain", codec = MixedCodec)]
enum Mixed {
    Unit,
    Tuple(u32, String),
    Struct { id: u64, name: String },
}

struct MixedCodec;

impl Codec<Mixed> for MixedCodec {
    type Error = std::convert::Infallible;

    fn encode(_event: &Mixed) -> Result<Bytes, Self::Error> {
        Ok(Bytes::new())
    }

    fn decode(_payload: &[u8]) -> Result<Mixed, Self::Error> {
        Ok(Mixed::Unit)
    }
}

/// Given an enum mixing unit, tuple, and struct arms, When `variant()` is
/// called, Then every arm form matches and its variant is the arm identifier
/// (the derive must ignore payload fields via `(..)` / `{ .. }`).
#[test]
fn enum_variant_handles_all_arm_shapes() {
    let unit = Mixed::Unit;
    let tuple = Mixed::Tuple(7, String::from("x"));
    let structured = Mixed::Struct {
        id: 9,
        name: String::from("y"),
    };

    assert_eq!(unit.variant().variant(), Some(Variant::new("Unit")));
    assert_eq!(tuple.variant().variant(), Some(Variant::new("Tuple")));
    assert_eq!(structured.variant().variant(), Some(Variant::new("Struct")));

    assert_eq!(unit.variant().type_key(), Mixed::EVENT_TYPE.type_key());
    assert_eq!(tuple.variant().type_key(), Mixed::EVENT_TYPE.type_key());
    assert_eq!(
        structured.variant().type_key(),
        Mixed::EVENT_TYPE.type_key()
    );
}

// generics: split_for_impl passes type params / where-clause through

// `inner` exists only to give the type a parameter for `split_for_impl` to pass
// through; the tests read `EVENT_TYPE` and `variant()`, never the field value.
//
// `T: Default` is the fixture's own requirement, not the derive's: the derive
// copies `codec = WrapperCodec` through verbatim and infers no bounds for it,
// so making one codec serve `Wrapper<T>` for every `T` needs a way to build a
// `T` on decode. Inferring that here is the job of whoever names the codec.
#[allow(dead_code)]
#[derive(Clone, Event)]
#[event(family = "generic", codec = WrapperCodec)]
struct Wrapper<T: Clone + Default + Send + Sync + 'static> {
    inner: T,
}

struct WrapperCodec;

impl<T: Clone + Default + Send + Sync + 'static> Codec<Wrapper<T>> for WrapperCodec {
    type Error = std::convert::Infallible;

    fn encode(_event: &Wrapper<T>) -> Result<Bytes, Self::Error> {
        Ok(Bytes::new())
    }

    fn decode(_payload: &[u8]) -> Result<Wrapper<T>, Self::Error> {
        Ok(Wrapper {
            inner: T::default(),
        })
    }
}

/// Given a generic struct that satisfies the `Event` supertrait bounds on its
/// type parameter, When the derive is applied, Then `EVENT_TYPE` uses the bare
/// type identifier (no generics in the name) and `variant()` works on a value.
#[test]
fn generic_struct_derive_uses_bare_type_name() {
    assert_eq!(
        <Wrapper<u32> as Event>::EVENT_TYPE,
        EventType::new(Family::new("generic"), TypeName::new("Wrapper")),
    );

    let value = Wrapper { inner: 1u32 };
    assert_eq!(value.variant(), <Wrapper<u32> as Event>::EVENT_TYPE);
    assert_eq!(value.variant().variant(), None);
}

// codec: the attribute value becomes the associated type, verbatim

/// Given each fixture names a codec in its `#[event(...)]` attribute, When the
/// derived `Event::Codec` is read back, Then it is exactly the type that was
/// named — across struct, empty-family, enum, data-bearing enum and generic
/// forms alike.
///
/// The check is at the type level: `codec_of` is satisfiable only by the one
/// associated type, so a derive that substituted a different codec, defaulted
/// to one, or inferred one from the shape of the event would fail to compile
/// here instead of writing an unexpected format at run time.
#[test]
fn codec_is_the_type_the_attribute_named() {
    codec_of::<Incremented, IncrementedCodec>();
    codec_of::<EmptyFamily, EmptyFamilyCodec>();
    codec_of::<OrderEvent, OrderEventCodec>();
    codec_of::<Mixed, MixedCodec>();
    codec_of::<Wrapper<u32>, WrapperCodec>();
}

// codec: the derive remains pure sugar

/// The same event written out by hand, naming the same codec.
#[derive(Clone)]
struct HandWritten;

impl Event for HandWritten {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("shop.orders"), TypeName::new("HandWritten"));

    type Codec = HandWrittenCodec;
}

struct HandWrittenCodec;

impl Codec<HandWritten> for HandWrittenCodec {
    type Error = std::convert::Infallible;

    fn encode(_event: &HandWritten) -> Result<Bytes, Self::Error> {
        Ok(Bytes::new())
    }

    fn decode(_payload: &[u8]) -> Result<HandWritten, Self::Error> {
        Ok(HandWritten)
    }
}

/// Given a hand-written `impl Event` that states `type Codec` directly, When it
/// is read the same way a derived one is, Then it satisfies the same contract —
/// the attribute is sugar for this line and nothing the derive adds is required
/// to make an event usable.
#[test]
fn a_hand_written_impl_names_its_codec_the_same_way() {
    codec_of::<HandWritten, HandWrittenCodec>();

    assert_eq!(
        HandWritten::EVENT_TYPE,
        EventType::new(Family::new("shop.orders"), TypeName::new("HandWritten"))
    );
    assert_eq!(HandWritten.variant(), HandWritten::EVENT_TYPE);
}
