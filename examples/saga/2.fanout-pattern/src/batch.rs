//! The aggregate that owns the decision.
//!
//! One decomposition is one decision, and a decision belongs to exactly one
//! stream.  Recording it as a single `BatchDecomposed` on the batch's own
//! stream is what keeps the fan-out inside the framework's axiom: an aggregate
//! is the consistency boundary, so anything that must be atomic has to fit in
//! one append to one stream.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use nitinol_eventsource::{Aggregate, Context, Decider, Effect, Event};
use nitinol_persistence::{EventType, Family, TypeName};

/// The fact event: "this batch was decomposed into these items".
///
/// # Why the event names its own batch
///
/// [`FanOutSaga`](crate::saga::FanOutSaga)'s
/// [`Saga::correlate`](nitinol_saga::Saga::correlate) receives the decoded
/// event and nothing else — not the stream key it came from — so an event that
/// omitted `batch` could not name the process instance it belongs to.  Carrying the
/// deciding stream's own key makes the trigger self-sufficient: every consumer
/// derives the whole fan-out from the record alone, without a side channel back
/// to where it was read from.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BatchDecomposed {
    /// Stream key of the batch that made this decision.
    pub batch: String,
    /// Stream keys of the children the decision calls into existence.
    pub items: Vec<String>,
}

impl Event for BatchDecomposed {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("saga.fanout"), TypeName::new("BatchDecomposed"));
}

/// The decision owner.
///
/// The aggregate keeps no state: the fan-out reads the decision from the
/// stream, never from this instance, so holding a copy here would be a second
/// place the same fact lives.
#[derive(Default)]
pub struct Batch;

impl Aggregate for Batch {
    type Event = BatchDecomposed;

    fn apply(&mut self, _event: BatchDecomposed) {}
}

/// Decompose a batch into the children named by `items`.
pub struct DecomposeBatch {
    pub items: Vec<String>,
}

#[async_trait]
impl Decider<DecomposeBatch> for Batch {
    type Rejection = std::convert::Infallible;

    async fn decide(
        &self,
        cmd: DecomposeBatch,
        ctx: &mut Context,
    ) -> Result<Effect<BatchDecomposed>, Self::Rejection> {
        // One decision, one event, one append — the whole atomicity the pattern
        // claims.  Splitting this into one event per child would spread the
        // decision over several appends and leave a crash able to record half
        // of it.
        Ok(Effect::persist(BatchDecomposed {
            batch: ctx.aggregate_id().as_str().to_owned(),
            items: cmd.items,
        }))
    }
}
