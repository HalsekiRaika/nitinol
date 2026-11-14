//! Which partition a stream key falls in.

use std::num::NonZeroUsize;

/// FNV-1a 64 offset basis.
const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;

/// FNV-1a 64 prime.
const PRIME: u64 = 0x0000_0100_0000_01b3;

/// The partition `stream_key` belongs to, out of `partitions`.
///
/// Every decision of one saga instance carries its stream key, so all of them
/// land in one partition, in order. The same key and count give the same
/// partition in every build and process.
pub fn partition_of(stream_key: &str, partitions: NonZeroUsize) -> usize {
    let mut hash = OFFSET_BASIS;
    for byte in stream_key.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    let modulus = u64::try_from(partitions.get()).expect("a partition count fits in u64");
    usize::try_from(hash % modulus)
        .expect("a remainder below the partition count fits in the usize that count came from")
}
