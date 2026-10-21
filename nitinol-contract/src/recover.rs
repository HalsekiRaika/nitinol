//! How an aggregate's state is reproduced before it decides anything.
//!
//! Two words, two mechanisms. *Replay* reads the aggregate's own stream and
//! applies every event on it. *Recover* starts from a snapshot the framework
//! holds and replays only what was written after it. Replay always happens;
//! whether a recover happens first is what the markers here say.

use crate::aggregate::Snapshotable;

mod sealed {
    pub trait Sealed {}
}

/// What [`Aggregate::Recover`](crate::Aggregate::Recover) may name.
///
/// Sealed: the two markers below are the whole of the choice, because each one
/// is a claim the framework has to be able to act on — a third would be a third
/// way to reproduce state that nothing implements.
///
/// `A` is the aggregate the marker was named on, which is what lets [`Latest`]
/// require [`Snapshotable`] of it and [`Never`] require nothing.
pub trait Recovery<A>: sealed::Sealed + Send + Sync + 'static {
    /// The choice this marker stands for, as a value.
    ///
    /// The choice itself is the type: a trait bound is what refuses an
    /// aggregate whose store holds no snapshots, and a value cannot do that.
    /// This is here so wiring that wants to *report* the choice has a way to
    /// read it, not as a second way to make it.
    const POLICY: RecoverPolicy;
}

/// The recovery each marker stands for, observed as a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoverPolicy {
    /// State is reproduced by replaying the stream from its beginning.
    Never,
    /// State is reproduced from the latest snapshot, then by replaying the
    /// events written after it.
    Latest,
}

/// Never recover: state is reproduced by replaying the whole stream.
///
/// Implemented for every aggregate, and asking nothing of the store beyond the
/// journal — so an aggregate that names this starts on any deployment's store.
pub struct Never;

/// Recover from the latest snapshot, then replay what was written after it.
///
/// Implemented only where the aggregate is [`Snapshotable`], because recovering
/// means reading a snapshot value back and that is the trait which says how.
/// Naming it on an aggregate that is not is a **compile error**:
///
/// ```compile_fail
/// # use bytes::Bytes;
/// # use nitinol_contract::{Aggregate, Codec, Event, Latest};
/// # use nitinol_persistence::{EventType, Family, TypeName};
/// # #[derive(Clone)]
/// # struct Noted;
/// # struct NotedCodec;
/// # impl Codec<Noted> for NotedCodec {
/// #     type Error = std::convert::Infallible;
/// #     fn encode(_: &Noted) -> Result<Bytes, Self::Error> { Ok(Bytes::new()) }
/// #     fn decode(_: &[u8]) -> Result<Noted, Self::Error> { Ok(Noted) }
/// # }
/// # impl Event for Noted {
/// #     const EVENT_TYPE: EventType = EventType::new(Family::new("doc"), TypeName::new("Noted"));
/// #     type Codec = NotedCodec;
/// # }
/// #[derive(Default)]
/// struct Ledger;
///
/// // `Ledger` is not `Snapshotable`, so there is no snapshot value to recover
/// // from and no impl of `Recovery<Ledger>` for `Latest` to satisfy this.
/// impl Aggregate for Ledger {
///     type Event = Noted;
///     type Recover = Latest;
///
///     fn apply(&mut self, _event: Noted) {}
/// }
/// ```
pub struct Latest;

impl sealed::Sealed for Never {}
impl sealed::Sealed for Latest {}

impl<A> Recovery<A> for Never {
    const POLICY: RecoverPolicy = RecoverPolicy::Never;
}

impl<A: Snapshotable> Recovery<A> for Latest {
    const POLICY: RecoverPolicy = RecoverPolicy::Latest;
}
