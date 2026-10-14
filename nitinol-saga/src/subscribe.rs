//! The alphabet of inputs a saga reacts to, and the typestate that binds a
//! subscription to each declared source.
//!
//! # Why the inputs are one declaration
//!
//! A saga is a coordinator: the failure of a business process is observed as a
//! reply event from the addressee *and* a deadline of the saga's own passing,
//! so a saga that could name only one upstream event type could not express the
//! thing it exists for.  [`SagaSubscribe`] is therefore the whole alphabet —
//! every external event type the saga observes, plus its own time — declared as
//! one user-defined enum.
//!
//! Declaring it as an enum is what makes a forgotten reaction a compile error:
//! [`Saga::correlate`](crate::Saga::correlate) and
//! [`Saga::handle`](crate::Saga::handle) both take it, so adding a source to the
//! alphabet leaves two `match`es non-exhaustive.
//!
//! # Why each source is observed on its own
//!
//! Observation is per source: each declared external event type carries a
//! subscription and a cursor of its own.  There is deliberately no total order
//! across sources — it is not a promise that can be kept across several upstream
//! streams in a distributed system — and the consequence that matters is the
//! useful one: a source that stalls does not stop another from progressing.
//!
//! # Why the bindings are a generated struct
//!
//! "Every declared source must be bound before `spawn` exists" is a typestate,
//! and the state is one slot per declared source.  A tuple of markers cannot
//! carry it: the slot-transition trait would have to be implemented for a
//! foreign tuple type in the user's crate, which the orphan rule forbids.  So
//! the derive generates a struct in the declaring crate, and
//! [`BindSource`] / [`AllSourcesSubscribed`] are implemented for it there.

use std::marker::PhantomData;
use std::sync::Arc;
use std::time::Duration;

use nitinol_eventsource::error::CodecError;
use nitinol_eventsource::{Event, SequenceCursor};
use nitinol_persistence::store::Journal;
use nitinol_persistence::EventType;

use crate::components::DeadlineMessage;
use crate::saga::Saga;

/// How a fired deadline is read back as the alphabet it belongs to.
///
/// Takes the identity the schedule fact persisted alongside the payload —
/// `message_type` and `message_codec`, [`DeadlineMessage::TYPE_NAME`] /
/// [`DeadlineMessage::CODEC_NAME`] at the time the deadline was set — so the
/// read-back can be checked against what this alphabet's deadline variant
/// currently declares before the bytes are decoded.
///
/// A plain `fn` pointer rather than a method, because the answer depends on the
/// alphabet alone and is therefore known at declaration time: an alphabet that
/// declares no deadline has none, which is exactly what
/// [`DeclaresDeadline::READ`] being `None` for [`NoDeadline`] says.
pub type ReadDeadline<A> = fn(&str, &str, &[u8]) -> Result<A, ReadDeadlineError>;

/// Why a firing's schedule fact could not be read back as the alphabet's
/// deadline variant.
#[derive(Debug, thiserror::Error)]
pub enum ReadDeadlineError {
    /// The schedule fact names a different message type or codec than this
    /// alphabet's `#[subscribe(deadline)]` variant currently declares.
    ///
    /// Reached only where the two can disagree: a schedule fact set by an
    /// earlier incarnation whose `DeadlineMessage` impl (or the alphabet
    /// naming it) has since changed. The bytes are never decoded under a
    /// declaration they were not written for — see [`DeadlineMessage`] for why
    /// the fact names itself rather than leaving this to be inferred.
    #[error(
        "schedule fact named `{stored_type}`/`{stored_codec}`, but this saga's \
         deadline variant currently declares `{declared_type}`/`{declared_codec}`"
    )]
    IdentityMismatch {
        stored_type: String,
        stored_codec: String,
        declared_type: &'static str,
        declared_codec: &'static str,
    },
    /// The identity matched, but the codec it names failed to decode the
    /// bytes.
    #[error(transparent)]
    Codec(#[from] CodecError),
}

/// Read a deadline message through the [`Codec`](nitinol_eventsource::codec::Codec)
/// its own type named, after checking that `message_type` / `message_codec` —
/// the identity the schedule fact persisted — are still what `M` declares.
///
/// Private: the only caller is [`HasDeadline`]'s [`DeclaresDeadline`] impl
/// below, which is also the only place a [`ReadDeadline`] is built. Neither the
/// derive nor a hand-written `SagaSubscribe` calls this directly any more —
/// they supply [`DeadlineVariant::wrap_deadline`], and this function is reached
/// through that impl rather than restated at each call site.
fn read_deadline_message<M: DeadlineMessage>(
    message_type: &str,
    message_codec: &str,
    payload: &[u8],
) -> Result<M, ReadDeadlineError> {
    if message_type != M::TYPE_NAME || message_codec != M::CODEC_NAME {
        return Err(ReadDeadlineError::IdentityMismatch {
            stored_type: message_type.to_owned(),
            stored_codec: message_codec.to_owned(),
            declared_type: M::TYPE_NAME,
            declared_codec: M::CODEC_NAME,
        });
    }
    nitinol_eventsource::codec::decode::<M::Codec, M>(payload).map_err(ReadDeadlineError::Codec)
}

/// The whole alphabet of inputs a saga reacts to.
///
/// Implemented by `#[derive(SagaSubscribe)]` on a user enum whose newtype
/// variants name the input types.  A variant marked `#[subscribe(deadline)]`
/// names the saga's own time; every other variant names an external event type
/// the saga observes as a source of its own.
///
/// A hand-written implementation is equally valid — the derive writes nothing a
/// person could not — but it has to produce a binding typestate with one slot
/// per external source, which is most of what the derive is for.
pub trait SagaSubscribe: Send + Sync + Sized + 'static {
    /// One binding slot per declared external source.
    ///
    /// The spawn builders carry this as a type parameter: binding a source moves
    /// its slot, and `spawn` exists only on the shape where every slot is
    /// filled.  [`Default`] is the all-unbound shape the builders start from.
    type Bindings: Default + Send + Sync + 'static;

    /// What this alphabet declares about its own time: [`NoDeadline`], or
    /// [`HasDeadline<M>`] for the message its `#[subscribe(deadline)]` variant
    /// names.
    ///
    /// [`SagaComponents::deadline::<M>()`](crate::SagaComponents::deadline),
    /// the spawn boundary's resident-timer resolution, and the read-back that
    /// turns a firing into this alphabet's deadline variant all read this one
    /// associated type — the first two through the equality bound `Deadline =
    /// HasDeadline<M>`, the read-back through [`DeclaresDeadline::READ`],
    /// which [`DeclaresDeadline`] *derives* from `Deadline` rather than from a
    /// fourth, independently-settable fact a hand-written `SagaSubscribe`
    /// could leave disagreeing with it. [`DeclaresDeadline`] is sealed to
    /// exactly [`NoDeadline`] and [`HasDeadline<M>`], and `HasDeadline<M>`'s
    /// only impl of it requires `Self: DeadlineVariant<M>` — so naming
    /// `HasDeadline<M>` here is only possible together with the
    /// [`DeadlineVariant<M>`] impl that makes the read-back real.
    type Deadline: DeclaresDeadline<Self>;
}

/// What [`SagaSubscribe::Deadline`] can be, and everything a reader needs to
/// act on it: whether the alphabet `A` has its own time, and — not as a
/// separate fact, but *derived from which shape `Deadline` is* — how a firing
/// is read back as `A`.
///
/// Sealed to [`NoDeadline`] and [`HasDeadline<M>`]. [`NoDeadline`]'s impl
/// applies for every `A` and fixes both consts; [`HasDeadline<M>`]'s impl
/// applies only where `A: DeadlineVariant<M>` and fixes
/// [`READ`](Self::READ) to `Some` of the one function built from that `wrap`.
/// Neither impl leaves either const for `A`'s own `SagaSubscribe` impl to set,
/// so there is no shape where [`HAS_DEADLINE`](Self::HAS_DEADLINE) is `true` —
/// `Deadline<M>` obtainable, resident timer core resolved — while `READ` is
/// `None`: a firing this saga can receive always has an arm it is delivered
/// to.
pub trait DeclaresDeadline<A: SagaSubscribe>: sealed::Sealed + Send + Sync + 'static {
    /// Type-erased over which message, if any, the alphabet declares: `true`
    /// for every [`HasDeadline<M>`], `false` for [`NoDeadline`].
    const HAS_DEADLINE: bool;

    /// How a fired deadline is read back as `A`'s deadline variant.
    ///
    /// `None` exactly when [`HAS_DEADLINE`](Self::HAS_DEADLINE) is `false`.
    ///
    /// The bytes are the ones the schedule fact persisted, checked against the
    /// identity the same fact names before being read through the
    /// [`Codec`](nitinol_eventsource::codec::Codec) the deadline message type
    /// named, not through a format the framework picked — see
    /// [`ReadDeadlineError::IdentityMismatch`].
    const READ: Option<ReadDeadline<A>>;
}

/// [`SagaSubscribe::Deadline`] for an alphabet with no `#[subscribe(deadline)]`
/// variant.
pub struct NoDeadline;

/// [`SagaSubscribe::Deadline`] for an alphabet whose `#[subscribe(deadline)]`
/// variant names message `M`.
///
/// Carries no data: the read-back lives on the [`DeclaresDeadline`] impl
/// below, built from the alphabet's [`DeadlineVariant<M>`] impl rather than
/// from a value this type or `A`'s `SagaSubscribe` impl could carry on its
/// own.
pub struct HasDeadline<M>(PhantomData<fn() -> M>);

mod sealed {
    pub trait Sealed {}
    impl Sealed for super::NoDeadline {}
    impl<M> Sealed for super::HasDeadline<M> {}
}

impl<A: SagaSubscribe> DeclaresDeadline<A> for NoDeadline {
    const HAS_DEADLINE: bool = false;
    const READ: Option<ReadDeadline<A>> = None;
}

impl<A, M> DeclaresDeadline<A> for HasDeadline<M>
where
    A: SagaSubscribe<Deadline = HasDeadline<M>> + DeadlineVariant<M>,
    M: DeadlineMessage,
{
    const HAS_DEADLINE: bool = true;
    const READ: Option<ReadDeadline<A>> = Some(|message_type, message_codec, payload| {
        read_deadline_message::<M>(message_type, message_codec, payload).map(A::wrap_deadline)
    });
}

/// How alphabet `A` constructs its `#[subscribe(deadline)]` variant from the
/// message `M` that variant names.
///
/// Written by `#[derive(SagaSubscribe)]` for the one variant marked
/// `#[subscribe(deadline)]`, or by hand for the same reason any
/// [`SagaSubscribe`] may be hand-written. This is the only way to satisfy
/// `HasDeadline<M>`'s [`DeclaresDeadline<A>`] impl, which is what ties
/// "`A`'s `Deadline` is `HasDeadline<M>`" to "a firing is read back into `A`":
/// there is no shape where the first holds and this impl is absent.
pub trait DeadlineVariant<M>: SagaSubscribe {
    /// Wrap a decoded deadline message as this alphabet's deadline variant.
    fn wrap_deadline(message: M) -> Self;
}

/// The binding typestate of `S`'s alphabet, in the shape the spawn builders
/// carry it.
///
/// Written out as a name because the projection it stands for appears in every
/// builder signature.
pub type SourceBindings<S> = <<S as Saga>::Subscribe as SagaSubscribe>::Bindings;

/// Marker: this declared source has no subscription bound yet.
///
/// Every slot of a freshly built [`SagaSubscribe::Bindings`] holds this, and
/// `spawn` is absent while any still does.
pub struct SubscriptionUnset;

/// Marker: this declared source has a subscription bound.
///
/// The field is the binding itself.  It is `pub` because the code the derive
/// writes moves one into a slot, and that code lives in the declaring crate;
/// it is not a way to mint one, because [`SourceBinding`] has no public
/// constructor.
pub struct SubscriptionSet(#[doc(hidden)] pub SourceBinding);

/// Where one declared source is read from, from which position, and how often.
///
/// Produced by
/// [`Subscription::into_binding`](crate::Subscription) and handed to the slot
/// the bound event type names.  It has no public constructor and no public
/// field: a user can move one the builder handed out, and cannot invent one.
pub struct SourceBinding {
    pub(crate) upstream_store: Journal,
    pub(crate) cursor: SequenceCursor,
    /// The cadence the binding named, where it named one.
    ///
    /// `None` leaves the interval `nitinol-eventsource` polls at, which is the
    /// sole owner of that default — restating it here would be a second place
    /// for it to be changed in.
    pub(crate) poll_interval: Option<Duration>,
}

/// Move the slot of the declared source `E` from unbound to bound.
///
/// Implemented by the derive once per declared external source, on the shape
/// whose slot for `E` is still [`SubscriptionUnset`] — so binding the same
/// source twice has no implementation to resolve to, and binding a source the
/// alphabet never declared has none either.
#[doc(hidden)]
pub trait BindSource<E> {
    /// The binding shape with `E`'s slot filled.
    type Bound;

    fn bind(self, binding: SourceBinding) -> Self::Bound;
}

/// Every declared external source of the alphabet `A` is bound.
///
/// Implemented by the derive on exactly one shape: the one with every slot
/// filled.  The spawn methods require it, which is how "a declared source left
/// unbound is a compile error" is stated.  A `#[subscribe(deadline)]` variant is
/// allotted no slot, so it asks for no binding.
#[doc(hidden)]
pub trait AllSourcesSubscribed<A> {
    /// The declared sources, in declaration order, each with the subscription
    /// bound to it.
    fn into_sources(self) -> Vec<SubscribedSource<A>>;
}

/// How the bytes of one declared source are read as the alphabet it belongs to.
///
/// Shared rather than owned because a poller's transform is rebuilt on every
/// supervised restart of the poller, and the alphabet is not `Clone`.
pub(crate) type DecodeSource<A> = Arc<dyn Fn(&[u8]) -> Result<A, CodecError> + Send + Sync>;

/// One declared source, resolved: which event type it reads, how that type is
/// read as the alphabet, and where it is read from.
///
/// This is what the spawn boundary turns into one poller per source.
#[doc(hidden)]
pub struct SubscribedSource<A> {
    event_type: EventType,
    decode: DecodeSource<A>,
    binding: SourceBinding,
}

impl<A: 'static> SubscribedSource<A> {
    /// Resolve the source `E` of the alphabet `A`.
    ///
    /// `wrap` is the alphabet's variant constructor for `E`, which is what makes
    /// a decoded record arrive at [`Saga::handle`](crate::Saga::handle) as the
    /// variant it belongs to rather than as a bare event.
    pub fn new<E>(bound: SubscriptionSet, wrap: fn(E) -> A) -> Self
    where
        E: Event,
    {
        Self {
            event_type: E::EVENT_TYPE,
            // A variant constructor is a plain `fn` pointer, so the closure
            // capturing it is `Send + Sync + 'static` without the alphabet
            // having to be.
            decode: Arc::new(move |payload: &[u8]| {
                nitinol_eventsource::codec::decode::<E::Codec, E>(payload).map(wrap)
            }),
            binding: bound.0,
        }
    }

    /// The declared source's event type: what a stored row has to name for this
    /// source to claim it.
    pub(crate) fn event_type(&self) -> EventType {
        self.event_type
    }

    /// Take the source apart into what the spawn boundary wires up: the decode
    /// this source reads with, and the binding that says where it reads from.
    pub(crate) fn into_parts(self) -> (DecodeSource<A>, SourceBinding) {
        (self.decode, self.binding)
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use nitinol_eventsource::codec::Codec;

    use super::*;

    #[derive(Debug)]
    struct Probe(u32);

    struct ProbeCodec;

    impl Codec<Probe> for ProbeCodec {
        type Error = std::convert::Infallible;

        fn encode(value: &Probe) -> Result<Bytes, Self::Error> {
            Ok(Bytes::from(value.0.to_le_bytes().to_vec()))
        }

        fn decode(payload: &[u8]) -> Result<Probe, Self::Error> {
            let bytes: [u8; 4] = payload.try_into().expect("4-byte payload");
            Ok(Probe(u32::from_le_bytes(bytes)))
        }
    }

    impl DeadlineMessage for Probe {
        const TYPE_NAME: &'static str = "probe";
        const CODEC_NAME: &'static str = "probe-codec";
        type Codec = ProbeCodec;
    }

    /// A schedule fact whose `message_type` / `message_codec` agree with what
    /// `Probe` currently declares reads back normally.
    #[test]
    fn matching_identity_decodes_through_the_named_codec() {
        let payload = Bytes::from(7u32.to_le_bytes().to_vec());

        let message = read_deadline_message::<Probe>("probe", "probe-codec", &payload)
            .expect("an identity matching the declaration must decode");

        assert_eq!(message.0, 7);
    }

    /// A schedule fact naming a different message type than `Probe` currently
    /// declares is refused outright — the bytes are never handed to
    /// `Probe::Codec`, so a coincidentally-compatible wire shape cannot slip
    /// through under the wrong declaration.
    #[test]
    fn a_mismatched_type_name_is_refused_before_the_codec_ever_runs() {
        let payload = Bytes::from(7u32.to_le_bytes().to_vec());

        let error = read_deadline_message::<Probe>("stale-probe", "probe-codec", &payload)
            .expect_err("a disagreeing message type must be refused, not decoded");

        match error {
            ReadDeadlineError::IdentityMismatch {
                stored_type,
                stored_codec,
                declared_type,
                declared_codec,
            } => {
                assert_eq!(stored_type, "stale-probe");
                assert_eq!(stored_codec, "probe-codec");
                assert_eq!(declared_type, Probe::TYPE_NAME);
                assert_eq!(declared_codec, Probe::CODEC_NAME);
            }
            ReadDeadlineError::Codec(_) => {
                panic!("a type-name mismatch must be reported as `IdentityMismatch`, not as a codec failure")
            }
        }
    }

    /// Same refusal for a disagreeing codec name — the two identity fields are
    /// checked together, so either one alone can trip the Fail Fast path.
    #[test]
    fn a_mismatched_codec_name_is_refused_before_the_codec_ever_runs() {
        let payload = Bytes::from(7u32.to_le_bytes().to_vec());

        let error = read_deadline_message::<Probe>("probe", "stale-codec", &payload)
            .expect_err("a disagreeing codec name must be refused, not decoded");

        assert!(matches!(error, ReadDeadlineError::IdentityMismatch { .. }));
    }
}
