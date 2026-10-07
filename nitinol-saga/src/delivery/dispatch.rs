//! One consequence of a decision, and what came back from carrying it out.

use futures_core::future::BoxFuture;

use nitinol_eventsource::error::Retryability;
use nitinol_eventsource::system::SystemHandle;
use nitinol_eventsource::{Aggregate, Decider};
use nitinol_persistence::AggregateId;

/// Why a delivery did not land, classified once.
///
/// Both halves are read off the answer a single `ask` gave, at the one place
/// that answer is in hand. A host that re-derived the classification later, or
/// picked the retryability from one failed delivery and the diagnostic from
/// another, could report a verdict it is not holding and hold on a failure it
/// reported as a verdict.
#[derive(Debug, Clone)]
pub(crate) struct DeliveryFailure {
    /// Whether re-issuing this delivery can plausibly reach another outcome.
    pub(crate) retryability: Retryability,
    /// What the receiver, or the machinery carrying the command to it, said.
    pub(crate) detail: String,
}

type Dispatch =
    Box<dyn FnOnce(SystemHandle) -> BoxFuture<'static, Result<(), DeliveryFailure>> + Send>;

/// One command, addressed to one aggregate.
///
/// A saga's mapping answers these, and a delivery host carries them out. The
/// command is held as the call that will issue it rather than as data: the
/// host writes no copy of it down, so nothing here has to be encodable, and a
/// command needs neither `Clone` nor a serializer to be delivered.
///
/// The target is kept beside the call so a host can say where a held delivery
/// was addressed without having to issue it.
pub struct SagaDelivery {
    target: AggregateId,
    dispatch: Dispatch,
}

impl SagaDelivery {
    /// Deliver `command` to the `A` identified by `target`.
    ///
    /// The bounds are the ones sending a command and waiting for its answer
    /// already asks for, and nothing beyond them: a delivery host persists no
    /// copy of a command, so it adds no encoding or cloning requirement of its
    /// own.
    pub fn new<A, C>(target: AggregateId, command: C) -> Self
    where
        A: Aggregate + Decider<C>,
        C: Send + Sync + 'static,
        <A as Decider<C>>::Output: Send + 'static,
        <A as Decider<C>>::Rejection: std::error::Error + Send + Sync + 'static,
    {
        let addressed = target.clone();
        Self {
            target,
            dispatch: Box::new(move |system: SystemHandle| {
                Box::pin(async move {
                    // Asking rather than telling is what makes a refusal
                    // something the host can see: a dispatch with no answer
                    // would leave "refused" and "carried out" the same event,
                    // and the record would be treated as finished either way.
                    match system.aggregate_proxy::<A>(addressed).ask(command).await {
                        Ok(_) => Ok(()),
                        Err(failure) => Err(DeliveryFailure {
                            retryability: failure.retryability(),
                            detail: failure.to_string(),
                        }),
                    }
                })
            }),
        }
    }

    /// The stream key this delivery is addressed to.
    pub fn target(&self) -> &AggregateId {
        &self.target
    }

    /// Issue the command and wait for the receiver's answer.
    pub(crate) async fn deliver(self, system: SystemHandle) -> Result<(), DeliveryFailure> {
        (self.dispatch)(system).await
    }
}
