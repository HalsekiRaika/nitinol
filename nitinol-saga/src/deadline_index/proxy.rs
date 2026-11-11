//! The handle to a running deadline index.

use nitinol_runtime::ident::Pid;

/// A handle to the deadline index a hosted saga type was started with, reached
/// through [`SagaHostProxy::deadline_index`](crate::SagaHostProxy::deadline_index).
///
/// The index is a reader, but one that keeps no offset: its cursors live in
/// its processes alone and are rebuilt from the start of the journal whenever
/// it starts — see the crate's [mental model](crate#mental-model).
///
/// The index has no operations of its own to offer: it writes nothing, so
/// there is no offset to move and nothing to stop it from losing. What it
/// answers is where it runs — one top-level process per partition, the same
/// count the delivery host is spread over.
#[derive(Clone, Debug)]
pub struct SagaDeadlineIndexProxy {
    partitions: Vec<Pid>,
}

impl SagaDeadlineIndexProxy {
    pub(crate) fn new(partitions: Vec<Pid>) -> Self {
        Self { partitions }
    }

    /// The runtime pid of each partition, in partition order.
    pub fn pids(&self) -> &[Pid] {
        &self.partitions
    }
}
