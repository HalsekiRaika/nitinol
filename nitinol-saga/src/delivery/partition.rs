//! Which partition a stream key falls in.

use std::num::NonZeroUsize;

/// FNV-1a 64 offset basis.
const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;

/// FNV-1a 64 prime.
const PRIME: u64 = 0x0000_0100_0000_01b3;

/// The partition `stream_key` belongs to, out of `partitions`.
///
/// Every decision of one saga instance carries that instance's stream key, so
/// hashing the key is what keeps an instance's decisions together: they all
/// land in one partition, and keeping the order inside a partition keeps the
/// order of the instance.
///
/// # Why the hash is written out here
///
/// The answer is part of a persisted key — a partition's checkpoint — so an
/// assignment that moved between two runs of one deployment would have a
/// partition resume from a position belonging to a different set of streams.
/// `std`'s default hasher promises nothing across builds or across processes:
/// its algorithm is explicitly allowed to change, and `RandomState` reseeds per
/// process. FNV-1a is specified by its two constants above, so the assignment
/// is fixed by this function's text rather than by the toolchain that compiled
/// it.
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
