//! Crate-private spawn environment.
//!
//! [`SpawnEnv`] captures the resolved parameters that are constant for all spawns
//! within a single call site (`ProcessSystem` or `ProcessContext`) — registry,
//! dead-letter proxy, default idle timeout, default capacities, and optional
//! parent Pid.
//!
//! Both `ProcessSystem` (top-level spawns, no parent) and `ProcessContext`
//! (child spawns, parent = self) construct a `SpawnEnv` and delegate to
//! `spawn`, so that `Props` decomposition, idle-timeout / capacity
//! resolution, and parent-Pid propagation are handled exactly once.

use std::time::Duration;

use crate::ident::Pid;
use crate::process::dead_letter::DeadLetterProxy;
use crate::process::driver::Driver;
use crate::process::lifecycle::{run, LifecycleConfig};
use crate::process::props::{
    resolve_idle_timeout, resolve_mailbox, resolve_pipe, resolve_stash, MailboxCapacity,
    PipeCapacity, Props, StashCapacity,
};
use crate::process::registry::ProcessRegistry;
use crate::process::{Process, ProcessProxy};

/// Resolved spawn context — the parameters common to all spawns within one
/// call site.
///
/// Construct via [`SpawnEnv::top_level`] for system-level spawns (no parent)
/// or [`SpawnEnv::child`] for child spawns (parent = self).
pub(crate) struct SpawnEnv {
    registry: ProcessRegistry,
    dead_letter: Option<DeadLetterProxy>,
    default_idle_timeout: Option<Duration>,
    default_mailbox_capacity: MailboxCapacity,
    default_stash_capacity: StashCapacity,
    default_pipe_capacity: PipeCapacity,
    parent: Option<Pid>,
}

impl SpawnEnv {
    /// Construct an environment for top-level (system) spawns: no parent,
    /// dead-letter proxy provided by the running `ProcessSystem`.
    pub(crate) fn top_level(
        registry: ProcessRegistry,
        dead_letter: DeadLetterProxy,
        default_idle_timeout: Option<Duration>,
        default_mailbox_capacity: MailboxCapacity,
        default_stash_capacity: StashCapacity,
        default_pipe_capacity: PipeCapacity,
    ) -> Self {
        Self {
            registry,
            dead_letter: Some(dead_letter),
            default_idle_timeout,
            default_mailbox_capacity,
            default_stash_capacity,
            default_pipe_capacity,
            parent: None,
        }
    }

    /// Construct an environment for child spawns: parent = the context's own
    /// Pid; registry and dead-letter are inherited from the parent process.
    pub(crate) fn child(
        registry: ProcessRegistry,
        dead_letter: Option<DeadLetterProxy>,
        default_idle_timeout: Option<Duration>,
        default_mailbox_capacity: MailboxCapacity,
        default_stash_capacity: StashCapacity,
        default_pipe_capacity: PipeCapacity,
        parent_pid: Pid,
    ) -> Self {
        Self {
            registry,
            dead_letter,
            default_idle_timeout,
            default_mailbox_capacity,
            default_stash_capacity,
            default_pipe_capacity,
            parent: Some(parent_pid),
        }
    }

    /// Construct an environment for detached (parentless) spawns with no
    /// system-level default idle timeout. Used for temporary one-shot processes
    /// (e.g. ask proxies) that are independent of any actor hierarchy.
    pub(crate) fn detached(
        registry: ProcessRegistry,
        dead_letter: Option<DeadLetterProxy>,
    ) -> Self {
        Self {
            registry,
            dead_letter,
            default_idle_timeout: None,
            default_mailbox_capacity: MailboxCapacity::Inherit,
            default_stash_capacity: StashCapacity::Inherit,
            default_pipe_capacity: PipeCapacity::Inherit,
            parent: None,
        }
    }

    pub(crate) fn registry(&self) -> &ProcessRegistry {
        &self.registry
    }

    pub(crate) async fn spawn<P: Process, D: Driver<P>>(
        &self,
        props: Props<P, D>,
    ) -> ProcessProxy<P> {
        let parts = props.into_parts();
        let timeout = resolve_idle_timeout(parts.idle_timeout, self.default_idle_timeout);
        let mailbox_capacity =
            resolve_mailbox(parts.mailbox_capacity, self.default_mailbox_capacity);
        let pipe_capacity = resolve_pipe(parts.pipe_capacity, self.default_pipe_capacity);
        let stash_capacity = resolve_stash(parts.stash_capacity, self.default_stash_capacity);
        run(LifecycleConfig {
            process: parts.initial,
            process_name: parts.name,
            registry: self.registry.clone(),
            mailbox_capacity,
            pipe_capacity,
            stash_capacity,
            driver: parts.driver,
            timeout,
            dead_letter: self.dead_letter.clone(),
            supervision: parts.supervision,
            parent: self.parent,
            default_idle_timeout: self.default_idle_timeout,
            default_mailbox_capacity: self.default_mailbox_capacity,
            default_stash_capacity: self.default_stash_capacity,
            default_pipe_capacity: self.default_pipe_capacity,
        })
        .await
    }
}
