use std::sync::Arc;

use tokio::sync::Notify;

/// The handle that wakes a process's driver out of its wait.
///
/// The lifecycle makes one waker per process. It hands the waker to the
/// process's driver tree through [`Driver::attach_waker`](crate::process::Driver::attach_waker)
/// and keeps the same waker in the process's
/// [`ProcessContext::waker`](crate::process::ProcessContext::waker), so a
/// handler can wake its own driver. Clones share one permit.
///
/// Like [`std::task::Waker`], a wake is not lost when no one is waiting: it is
/// kept, and the next [`woken`](Self::woken) completes at once. Wakes given
/// before that are folded into one, so waking N times wakes the driver once.
/// A `woken` future that took a wake and is then dropped unfinished — as the
/// lifecycle loop drops the losing branch of its `select!` — hands the wake on
/// to the next `woken`, so it is not lost either.
///
/// There is one waker per process, not one per driver: when several drivers
/// in a combined driver tree honour it, a single wake reaches only one of
/// them.
#[derive(Clone)]
pub struct ProcessWaker(Arc<Notify>);

impl ProcessWaker {
    pub(crate) fn new() -> Self {
        Self(Arc::new(Notify::new()))
    }

    /// Wakes the driver waiting on [`woken`](Self::woken), or, when none is
    /// waiting, has the next `woken` complete at once. Does not block.
    pub fn wake(&self) {
        self.0.notify_one();
    }

    /// Waits until the next [`wake`](Self::wake), or completes at once when a
    /// wake was given while no one was waiting.
    pub async fn woken(&self) {
        self.0.notified().await;
    }
}

#[cfg(test)]
mod tests {
    use futures_util::FutureExt;

    use super::ProcessWaker;

    #[test]
    fn wakes_with_no_one_waiting_are_kept_and_folded_into_one() {
        let waker = ProcessWaker::new();

        waker.wake();
        waker.wake();
        waker.wake();

        assert!(
            waker.woken().now_or_never().is_some(),
            "a wake given while no one waits must complete the next woken at once"
        );
        assert!(
            waker.woken().now_or_never().is_none(),
            "three wakes must fold into one permit, leaving none for a second woken"
        );
    }
}
