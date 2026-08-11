use std::collections::VecDeque;
use std::future::Future;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};

use crate::error::HandlerError;
use crate::process::driver::Driver;
use crate::process::task::UserTask;
use crate::process::{Process, ProcessContext};

struct StashInner<P: Process> {
    capacity: NonZeroUsize,
    stashed: VecDeque<UserTask<P>>,
    ready: VecDeque<UserTask<P>>,
}

pub(crate) struct StashDriver<P: Process> {
    inner: Arc<Mutex<StashInner<P>>>,
}

impl<P: Process> StashDriver<P> {
    pub(crate) fn new(capacity: NonZeroUsize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(StashInner {
                capacity,
                stashed: VecDeque::new(),
                ready: VecDeque::new(),
            })),
        }
    }

    pub(crate) fn handle(&self) -> StashHandle<P> {
        StashHandle {
            inner: self.inner.clone(),
        }
    }
}

impl<P: Process> Driver<P> for StashDriver<P> {
    type Event = UserTask<P>;

    fn next(&mut self) -> impl Future<Output = Option<Self::Event>> + Send {
        let popped = self
            .inner
            .lock()
            .expect("stash lock was poisoned by a panicking holder")
            .ready
            .pop_front();
        async move {
            match popped {
                Some(task) => Some(task),
                None => std::future::pending().await,
            }
        }
    }

    async fn apply(
        &mut self,
        state: &mut P,
        ctx: &mut ProcessContext<P>,
        ev: Self::Event,
    ) -> Result<(), HandlerError> {
        ev.run(state, ctx).await
    }
}

pub(crate) struct StashHandle<P: Process> {
    inner: Arc<Mutex<StashInner<P>>>,
}

impl<P: Process> Clone for StashHandle<P> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<P: Process> StashHandle<P> {
    pub(crate) fn try_stash(&self, task: UserTask<P>) -> Result<(), UserTask<P>> {
        let mut inner = self
            .inner
            .lock()
            .expect("stash lock was poisoned by a panicking holder");
        if inner.stashed.len() >= inner.capacity.get() {
            return Err(task);
        }
        inner.stashed.push_back(task);
        Ok(())
    }

    pub(crate) fn unstash(&self, n: usize) {
        let mut inner = self
            .inner
            .lock()
            .expect("stash lock was poisoned by a panicking holder");
        let count = n.min(inner.stashed.len());
        for _ in 0..count {
            let task = inner
                .stashed
                .pop_front()
                .expect("count is bounded by stashed.len()");
            inner.ready.push_back(task);
        }
    }

    pub(crate) fn unstash_all(&self) {
        let mut inner = self
            .inner
            .lock()
            .expect("stash lock was poisoned by a panicking holder");
        let drained = std::mem::take(&mut inner.stashed);
        inner.ready.extend(drained);
    }
}
