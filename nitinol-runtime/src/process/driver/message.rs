use std::future::Future;

use tokio::sync::mpsc;

use crate::error::HandlerError;
use crate::process::driver::Driver;
use crate::process::task::UserTask;
use crate::process::{Process, ProcessContext};

/// `Driver<P>` over the `mpsc::Receiver<UserTask<P>>` that backs `tell` /
/// `ask`. This is the only driver wired in by `ProcessSystem::spawn*`; other
/// `Driver` impls live in sibling crates (e.g. the planned `IntervalDriver`
/// in nitinol-eventsource, #49).
pub(crate) struct MessageDriver<P: Process> {
    rx: mpsc::Receiver<UserTask<P>>,
}

impl<P: Process> MessageDriver<P> {
    pub(crate) fn new(rx: mpsc::Receiver<UserTask<P>>) -> Self {
        Self { rx }
    }
}

impl<P: Process> Driver<P> for MessageDriver<P> {
    type Event = UserTask<P>;

    fn next(&mut self) -> impl Future<Output = Option<Self::Event>> + Send {
        self.rx.recv()
    }

    async fn apply(
        &mut self,
        state: &mut P,
        ctx: &mut ProcessContext,
        ev: Self::Event,
    ) -> Result<(), HandlerError> {
        ev.run(state, ctx).await
    }
}
