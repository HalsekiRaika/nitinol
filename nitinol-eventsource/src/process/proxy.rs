use nitinol_runtime::process::ProcessProxy;
use nitinol_runtime::error::AskError as RuntimeAskError;

use crate::aggregate::Aggregate;
use crate::decider::Decider;
use crate::error::{AskError, AskHandlerError, ExecError, ExecHandlerError, TellError};
use crate::process::aggregate_process::{AggregateProcess, AskCmd, ExecMsg};
use crate::receive::Receive as EvtReceive;

/// A typed handle to an `AggregateProcess<A>` with a high-level domain API.
///
/// Wraps `ProcessProxy<AggregateProcess<A>>` so callers interact with domain
/// types (`ask`, `tell`, `exec`) rather than runtime plumbing.
pub struct AggregateProxy<A: Aggregate>(pub(crate) ProcessProxy<AggregateProcess<A>>);

impl<A: Aggregate> Clone for AggregateProxy<A> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<A: Aggregate> AggregateProxy<A> {
    /// Send a command and wait for the persisted events.
    ///
    /// Returns `Vec<A::Event>` containing every event produced and applied by
    /// `Decider::decide`.  Side effects are excluded (fire-and-forget).
    pub async fn ask<C>(
        &self,
        cmd: C,
    ) -> Result<Vec<A::Event>, AskError<<A as Decider<C>>::Rejection>>
    where
        A: Decider<C>,
        C: Send + Sync + 'static,
    {
        self.0
            .ask(AskCmd(cmd))
            .await
            .map_err(map_ask_error)
    }

    /// Send a command without waiting for a response.
    ///
    /// The command is queued and processed in FIFO order; ordering relative to
    /// subsequent `exec` calls is guaranteed by the single-threaded process loop.
    pub async fn tell<C>(&self, cmd: C) -> Result<(), TellError>
    where
        A: Decider<C>,
        C: Send + Sync + 'static,
    {
        self.0.tell(AskCmd(cmd)).await.map_err(TellError::Send)
    }

    /// Send a read-only query and wait for the response.
    ///
    /// `exec` does not mutate aggregate state.
    pub async fn exec<M>(
        &self,
        msg: M,
    ) -> Result<<A as EvtReceive<M>>::Response, ExecError<<A as EvtReceive<M>>::Error>>
    where
        A: EvtReceive<M>,
        M: Send + Sync + 'static,
    {
        self.0
            .ask(ExecMsg(msg))
            .await
            .map_err(map_exec_error)
    }
}

// ---------------------------------------------------------------------------
// Error mappers
// ---------------------------------------------------------------------------

fn map_ask_error<R>(
    e: RuntimeAskError<AskHandlerError<R>>,
) -> AskError<R>
where
    R: std::error::Error + Send + Sync + 'static,
{
    match e {
        RuntimeAskError::Handler(h) => match h {
            AskHandlerError::Rejection(r) => AskError::Rejection(r),
            AskHandlerError::Effect(eff) => AskError::Effect(eff),
        },
        RuntimeAskError::DeadLetter { .. } | RuntimeAskError::ReplyDropped => {
            AskError::Send(nitinol_runtime::error::SendError)
        }
    }
}

fn map_exec_error<E>(
    e: RuntimeAskError<ExecHandlerError<E>>,
) -> ExecError<E>
where
    E: std::error::Error + Send + Sync + 'static,
{
    match e {
        RuntimeAskError::Handler(h) => match h {
            ExecHandlerError::Domain(e) => ExecError::Domain(e),
        },
        RuntimeAskError::DeadLetter { .. } | RuntimeAskError::ReplyDropped => {
            ExecError::Send(nitinol_runtime::error::SendError)
        }
    }
}
