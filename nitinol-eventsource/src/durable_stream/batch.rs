/// What one read of the journal returned, handed to a batch subscriber as one
/// message.
///
/// `records` holds each record the subscription's transform kept, paired with
/// the position the poller's cursor observed it at, in ascending order. A
/// record the transform dropped is not in the list, but it was read, so
/// `last_observed` — the position of the last record read — can lie past the
/// last entry.
///
/// Framework plumbing between a poller and the projector it feeds; nothing
/// outside this crate is expected to build one.
#[derive(Debug)]
pub struct PolledBatch<T> {
    /// The records the transform kept, each paired with the position it was
    /// read at.
    pub records: Vec<(u64, T)>,
    /// The position of the last record read.
    pub last_observed: u64,
}

/// The answer to a [`PolledBatch`]: every record up to and including this
/// position is settled, and the poller moves its cursor here.
///
/// It equals the batch's `last_observed` when the whole batch settled, and is
/// an earlier position when the subscriber stopped part-way. A batch of which
/// nothing settled is answered with an error instead, so the cursor stays put.
///
/// The message and its answer are one exchange, so they are defined together.
#[derive(Debug, Clone, Copy)]
pub struct SettledThrough(pub u64);
