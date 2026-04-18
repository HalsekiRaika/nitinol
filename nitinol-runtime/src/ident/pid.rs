use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_PID: AtomicU64 = AtomicU64::new(0);

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
