//! What a delivery host refuses an operator.

/// Why an operator's move of a partition's recorded progress did not happen.
///
/// Every variant leaves the partition exactly as it was: the stored position,
/// the position the running partition is reading from, and what it is held at
/// are all unchanged.
#[derive(Debug, thiserror::Error)]
pub enum SagaDeliveryError {
    /// The host has no such partition.
    ///
    /// Partitions are numbered from zero, so a host of `partitions` has the
    /// numbers below it and no others.
    #[error(
        "this delivery host has {partitions} partition(s), so there is no partition {partition}"
    )]
    NoSuchPartition { partition: usize, partitions: usize },
    /// The move would not carry the partition's progress forward.
    ///
    /// Moving on is a declaration that the deliveries in the stretch passed
    /// over will not be made. Moving back would be a declaration that a
    /// stretch already finished with should be delivered again, which is a
    /// different operation and not this one.
    #[error(
        "partition {partition} has already finished with {reached}, so moving it to {to} would \
         not carry its progress forward"
    )]
    NotForward {
        partition: usize,
        to: u64,
        reached: u64,
    },
    /// The store would not take the new position.
    #[error("recording {to} as partition {partition}'s progress failed: {detail}")]
    NotRecorded {
        partition: usize,
        to: u64,
        detail: String,
    },
    /// The partition could not be reached.
    ///
    /// Nothing was asked of the store, because the partition that owns the
    /// decision never heard the request.
    #[error("partition {partition} could not be reached")]
    Unreachable { partition: usize },
}
