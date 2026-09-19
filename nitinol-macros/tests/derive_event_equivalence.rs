//! Behavioral equivalence tests for `#[derive(Event)]`.
//!
//! The derive is pure sugar: its expansion must be *functionally equivalent* to
//! the hand-written `impl Event` pattern. The naming rule is "family = the
//! attribute's family, type name = the type identifier", and it is the whole of
//! what a derived identity is — an enum is named after the enum, not after
//! whichever arm a value happens to hold, because the arm is the payload's to
//! say.
//!
//! Both the `Event` trait and the `Event` derive are reached through the single
//! umbrella path `nitinol::eventsource::Event` (trait namespace vs. macro
//! namespace). This is the compatibility re-export path; the generated code
//! itself references the canonical `::nitinol::contract::Event` and
//! `::nitinol::persistence::{...}`.
//!
//! `Event` also names the codec that writes the event down, so equivalence
//! covers a second generated item: `#[event(codec = X)]` must produce
//! `type Codec = X` and nothing else. Each fixture below names a codec of its
//! own, and `codec_is_the_type_the_attribute_named` reads the association back
//! at the type level — a derive that substituted, defaulted or inferred a codec
//! would fail there rather than silently write the wrong format.

use bytes::Bytes;
use nitinol::contract::Codec;
use nitinol::eventsource::Event;
use nitinol::persistence::{EventType, Family, TypeName};

/// Passes only if `E`'s associated codec *is* `C`, not merely some type that
/// could encode `E`.
fn codec_of<E, C>()
where
    E: Event<Codec = C>,
{
}

// struct: family + type identifier

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
}

// struct: empty family

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

    // Display omits the empty family prefix (persistence contract), proving the
    // derived const carries a genuinely empty family.
    assert_eq!(format!("{}", EmptyFamily::EVENT_TYPE), "EmptyFamily");
}

// enum: one identity, named after the enum

// The arms exist so the derive has more than one to expand over; the tests read
// the declaration's identity, never a value.
#[allow(dead_code)]
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
/// Then it is the identity named after the enum — the same shape a struct gets,
/// because which arm a value holds is the payload's business.
#[test]
fn enum_event_type_is_named_after_the_enum() {
    let expected = EventType::new(Family::new("saga.upstream"), TypeName::new("OrderEvent"));

    assert_eq!(OrderEvent::EVENT_TYPE, expected);
    assert_eq!(
        OrderEvent::EVENT_TYPE.to_path().to_string(),
        "saga.upstream.OrderEvent",
        "a row of any arm carries this path, so the enum is written down once"
    );
}

// enum: unit / tuple / struct arms all expand; arm fields are ignored

// Arm payloads exist only so the derive has tuple/struct shapes to expand over;
// the tests read the identity, never the field values.
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

/// Given an enum mixing unit, tuple, and struct arms, When the derive is
/// applied, Then it expands over every arm shape and produces the one identity
/// named after the enum.
#[test]
fn enum_with_every_arm_shape_derives_one_identity() {
    assert_eq!(
        Mixed::EVENT_TYPE,
        EventType::new(Family::new("domain"), TypeName::new("Mixed"))
    );
}

// generics: split_for_impl passes type params / where-clause through

// `inner` exists only to give the type a parameter for `split_for_impl` to pass
// through; the tests read `EVENT_TYPE`, never the field value.
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
/// type identifier (no generics in the name).
#[test]
fn generic_struct_derive_uses_bare_type_name() {
    assert_eq!(
        <Wrapper<u32> as Event>::EVENT_TYPE,
        EventType::new(Family::new("generic"), TypeName::new("Wrapper")),
    );
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
/// the attribute is sugar for these two lines and nothing the derive adds is
/// required to make an event usable.
#[test]
fn a_hand_written_impl_names_its_codec_the_same_way() {
    codec_of::<HandWritten, HandWrittenCodec>();

    assert_eq!(
        HandWritten::EVENT_TYPE,
        EventType::new(Family::new("shop.orders"), TypeName::new("HandWritten"))
    );
}
