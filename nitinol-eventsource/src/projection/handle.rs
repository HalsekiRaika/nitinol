use nitinol_runtime::process::ProcessProxy;

use crate::projection::activator::{IsActive, ProjectionActivator, Unregister};
use crate::projection::ProjectionId;

/// A handle onto a projection spawned on a system.
///
/// It names the projection, not a process: the projector behind it may stop on
/// its idle timeout and be started again by the system's projection activator
/// any number of times, and the handle names it throughout. Every clone, and
/// the handle any later spawn of the same projection id returns, names the
/// same projection.
#[derive(Clone)]
pub struct ProjectorHandle {
    projection_id: ProjectionId,
    activator: ProcessProxy<ProjectionActivator>,
}

impl ProjectorHandle {
    pub(crate) fn new(
        projection_id: ProjectionId,
        activator: ProcessProxy<ProjectionActivator>,
    ) -> Self {
        Self {
            projection_id,
            activator,
        }
    }

    /// Whether a projector is running for this projection now.
    ///
    /// `false` while the projection is passivated — its projector stopped on
    /// its idle timeout, to be activated again by the next event it follows —
    /// and after [`stop`](Self::stop).
    ///
    /// Asked of the activator rather than read off a process: the handle names
    /// a projection, not a process, and the activator is what knows which
    /// projector, if any, runs for it.
    pub async fn is_active(&self) -> bool {
        // The activator is unreachable only while the process system shuts
        // down, its projectors with it.
        self.activator
            .ask(IsActive(self.projection_id.clone()))
            .await
            .unwrap_or(false)
    }

    /// Take the projection out of its system's activator and stop its
    /// projector, returning once the projector has ended.
    ///
    /// Nothing appended afterwards activates it again; spawning the projection
    /// again registers it anew.
    pub async fn stop(&self) {
        let Ok(Some(stopped)) = self
            .activator
            .ask(Unregister(self.projection_id.clone()))
            .await
        else {
            // Either no projector was running, or the activator has already
            // ended, which only the process system shutting down does.
            return;
        };
        // Dropped unanswered only if the activator ended, which only the
        // process system shutting down does.
        let _ = stopped.await;
    }
}
