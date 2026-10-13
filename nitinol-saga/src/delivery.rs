//! The delivery host: carrying a saga's decisions to the aggregates they are
//! addressed to.
//!
//! A saga persists what it decided. Something then has to turn each decision
//! into the commands it implies and get them to their targets. That is this
//! module: a resident consumer of the saga's own stream, pulling rather than
//! being pushed to, with its progress kept where a restart can find it.
//!
//! # What it reads, and what it owes
//!
//! A host subscribes across every instance of one saga type — one stream per
//! instance — by naming the declared event type to the store together with the
//! position it has finished with. The store does the selecting, so no row of
//! another type is fetched to be thrown away, and the subscription is not tied
//! to any one stream.
//!
//! A record is finished with when **every** delivery it implied landed.
//! Anything less and the position stays below it: a cursor that moved past a
//! delivery still owed would leave that delivery with nothing pointing at it.
//!
//! # Why a command is asked rather than told
//!
//! The receiver's answer is the only thing that says a command was carried
//! out. Sending without waiting would make "the decider refused it" and "the
//! decider carried it out" the same event at the host, and the record would be
//! treated as finished either way.
//!
//! A collision with an aggregate that already exists is **not** read as
//! success. What such a collision carries is the fact of the collision and
//! nothing else — which operation wrote the position that is taken is not in
//! it — so the host cannot tell its own redelivery from somebody else's first
//! write. The one success signal it accepts is the receiver's own answer.
//!
//! # Failure is a hold, not a skip
//!
//! Nothing is ever passed over because it failed, however permanently. A
//! transient failure is retried with a growing pause; a permanent refusal is
//! held and said out loud. Passing over a delivery is a judgement about the
//! business, so it is an operator's to make through
//! [`SagaDeliveryProxy::advance_offset`] — and a hold is confined to one
//! partition so that it stops as little as possible while they decide.

mod dispatch;
mod error;
mod id;
mod outcome;
mod partition;
mod process;
mod props;
mod proxy;

pub use self::dispatch::SagaDelivery;
pub use self::error::SagaDeliveryError;
pub use self::id::SagaDeliveryId;
pub use self::partition::partition_of;
pub use self::props::SagaDeliveryProps;
pub use self::proxy::SagaDeliveryProxy;
