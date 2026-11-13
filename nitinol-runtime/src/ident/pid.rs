use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_PID: AtomicU64 = AtomicU64::new(0);

/// The identifier of a process.
///
/// Each spawned process gets a new `Pid`. No two processes in the same program
/// share a `Pid`, even in different [`ProcessSystem`](crate::ProcessSystem)s,
/// and a `Pid` is not reused after its process stops. A `Pid` is not unique
/// across separate runs of the program or across machines. It displays as a
/// plain number.
#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash)]
pub struct Pid(u64);

impl Pid {
    pub(crate) fn next() -> Self {
        Self(NEXT_PID.fetch_add(1, Ordering::Relaxed))
    }
}

impl AsRef<u64> for Pid {
    fn as_ref(&self) -> &u64 {
        &self.0
    }
}

impl std::fmt::Display for Pid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}
