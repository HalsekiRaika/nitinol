use std::any::TypeId;
use std::collections::HashMap;
use std::time::{Duration, Instant};

const THROTTLE_WINDOW: Duration = Duration::from_secs(10);
const THROTTLE_LIMIT: u32 = 10;

struct WindowCounter {
    count: u32,
    window_start: Instant,
}

pub(crate) struct LogThrottle {
    counters: HashMap<TypeId, WindowCounter>,
}

impl LogThrottle {
    pub(crate) fn new() -> Self {
        Self {
            counters: HashMap::new(),
        }
    }

    /// Returns `true` if further log output for this type should be suppressed.
    /// Increments the counter; resets the window when expired.
    pub(crate) fn check_throttle(&mut self, type_id: TypeId) -> bool {
        let now = Instant::now();
        let counter = self
            .counters
            .entry(type_id)
            .or_insert_with(|| WindowCounter {
                count: 0,
                window_start: now,
            });
        if now.duration_since(counter.window_start) >= THROTTLE_WINDOW {
            counter.count = 0;
            counter.window_start = now;
        }
        counter.count += 1;
        counter.count > THROTTLE_LIMIT
    }
}
