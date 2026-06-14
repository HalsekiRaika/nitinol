use crate::ident::ProcessName;
use crate::process::driver::DynDriver;
use crate::process::props::capacity::{MailboxCapacity, PipeCapacity, StashCapacity};
use crate::process::props::idle_timeout::IdleTimeout;
use crate::process::supervision::SupervisionConfig;
use crate::process::Process;

/// All fields a `Props<P>` carries, decomposed into a named struct so the
/// spawn boundary can move them around without 8-arg positional plumbing.
///
/// Replaces the pre-spec `(P, IdleTimeout, SupervisionConfig<P>)` tuple
/// (`P10`). Fields are `pub(crate)` and intentionally non-`Default`: adding a
/// new axis requires touching every consumer once, which is the regression
/// guarantee the named struct buys vs. an extensible tuple.
pub(crate) struct PropsParts<P: Process> {
    pub(crate) initial: P,
    pub(crate) supervision: SupervisionConfig<P>,
    pub(crate) idle_timeout: IdleTimeout,
    pub(crate) mailbox_capacity: MailboxCapacity,
    pub(crate) stash_capacity: StashCapacity,
    pub(crate) pipe_capacity: PipeCapacity,
    pub(crate) custom_drivers: Vec<Box<dyn DynDriver<P>>>,
    pub(crate) name: Option<ProcessName>,
}
