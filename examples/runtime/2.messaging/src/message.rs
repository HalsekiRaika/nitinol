/// Fire-and-forget command: increments the counter by 1.
pub struct Increment;

/// Fire-and-forget command: decrements the counter by 1 (saturates at 0).
pub struct Decrement;

/// Request-response query: returns the current counter value.
pub struct GetCount;
