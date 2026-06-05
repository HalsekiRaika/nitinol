//! Crate-private spawn environment.
//!
//! [`SpawnEnv`] captures the resolved parameters that are constant for all spawns
//! within a single call site (`ProcessSystem` or `ProcessContext`) — registry,
//! dead-letter proxy, default idle timeout, and optional parent Pid.
//!
//! Both `ProcessSystem` (top-level spawns, no parent) and `ProcessContext`
//! (child spawns, parent = self) construct a `SpawnEnv` and delegate to
//! `spawn` / `spawn_with_driver`, so that `Props` decomposition,
//! idle-timeout resolution, and parent-Pid propagation are handled exactly once.

use std::time::Duration;

use tokio::sync::mpsc;

use crate::ident::{Pid, ProcessName};
use crate::process::dead_letter::DeadLetterProxy;
use crate::process::driver::Driver;
use crate::process::lifecycle::{run, run_with_driver, LifecycleInit};
use crate::process::props::{resolve_idle_timeout, Props};
use crate::process::registry::ProcessRegistry;
use crate::process::task::UserTask;
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
    parent: Option<Pid>,
}

impl SpawnEnv {
    /// Construct an environment for top-level (system) spawns: no parent,
    /// dead-letter proxy provided by the running `ProcessSystem`.
    pub(crate) fn top_level(
        registry: ProcessRegistry,
        dead_letter: DeadLetterProxy,
        default_idle_timeout: Option<Duration>,
    ) -> Self {
        Self {
            registry,
            dead_letter: Some(dead_letter),
            default_idle_timeout,
            parent: None,
        }
    }

    /// Construct an environment for child spawns: parent = the context's own
    /// Pid; registry and dead-letter are inherited from the parent process.
    pub(crate) fn child(
        registry: ProcessRegistry,
        dead_letter: Option<DeadLetterProxy>,
        default_idle_timeout: Option<Duration>,
        parent_pid: Pid,
    ) -> Self {
        Self {
            registry,
            dead_letter,
            default_idle_timeout,
            parent: Some(parent_pid),
        }
    }

    pub(crate) async fn spawn<P: Process>(
        &self,
        name: Option<ProcessName>,
        props: Props<P>,
    ) -> ProcessProxy<P> {
        let (process, idle_timeout, supervision) = props.into_parts();
        let timeout = resolve_idle_timeout(idle_timeout, self.default_idle_timeout);
        run(
            process,
            name,
            self.registry.clone(),
            timeout,
            self.dead_letter.clone(),
            supervision,
            LifecycleInit {
                parent: self.parent,
                default_idle_timeout: self.default_idle_timeout,
            },
        )
        .await
    }

    pub(crate) async fn spawn_with_driver<P: Process, D: Driver<P>>(
        &self,
        name: Option<ProcessName>,
        props: Props<P>,
        driver: D,
    ) -> ProcessProxy<P> {
        let (process, idle_timeout, supervision) = props.into_parts();
        let timeout = resolve_idle_timeout(idle_timeout, self.default_idle_timeout);
        let (user_tx, _user_rx) = mpsc::channel::<UserTask<P>>(32);
        run_with_driver(
            process,
            name,
            self.registry.clone(),
            user_tx,
            driver,
            timeout,
            self.dead_letter.clone(),
            supervision,
            LifecycleInit {
                parent: self.parent,
                default_idle_timeout: self.default_idle_timeout,
            },
        )
        .await
    }
}
