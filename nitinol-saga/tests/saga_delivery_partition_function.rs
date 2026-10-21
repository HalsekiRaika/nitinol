//! Which partition a stream key falls in.
//!
//! ```gherkin
//!   Scenario: one key, one partition
//!     Given a stream key and a partition count
//!     When the partition is computed repeatedly
//!     Then the answer is always the same number
//!
//!   Scenario: the assignment survives a restart
//!     Given a stream key whose partition was computed before
//!     When it is computed again in a later process
//!     Then the answer is the same number
//! ```
//!
//! The second scenario is why the expected numbers below are written out as
//! literals rather than derived. An offset is per partition and persists, so
//! an assignment that changed between two runs of the same deployment would
//! have a partition resume from a position belonging to a different set of
//! streams. A hash whose algorithm is only promised for the lifetime of one
//! process cannot keep that, and the literals are what refuse it: they were
//! computed once from the declared constants, and nothing but a change of
//! algorithm moves them.

use std::num::NonZeroUsize;

use nitinol_saga::partition_of;

fn partitions(n: usize) -> NonZeroUsize {
    NonZeroUsize::new(n).expect("a fixture partition count is not zero")
}

#[test]
fn a_key_falls_in_the_partition_the_declared_hash_puts_it_in() {
    let four = partitions(4);

    assert_eq!(partition_of("instance-1", four), 0);
    assert_eq!(partition_of("instance-2", four), 1);
    assert_eq!(partition_of("instance-3", four), 2);
    assert_eq!(partition_of("a", four), 0);
    assert_eq!(
        partition_of("", four),
        1,
        "the empty key is hashed like any other, from the offset basis alone"
    );
}

#[test]
fn widening_the_partition_count_reassigns_by_the_same_hash() {
    let eight = partitions(8);

    assert_eq!(partition_of("instance-1", eight), 4);
    assert_eq!(partition_of("instance-2", eight), 5);
    assert_eq!(partition_of("instance-3", eight), 2);
}

#[test]
fn one_key_always_answers_the_same_partition() {
    let four = partitions(4);
    let first = partition_of("instance-1", four);

    for _ in 0..64 {
        assert_eq!(
            partition_of("instance-1", four),
            first,
            "the assignment is derived from the key and the count alone, so it \
             cannot depend on anything seeded per process or per call"
        );
    }
}

#[test]
fn a_single_partition_takes_every_key() {
    let one = partitions(1);

    for key in ["instance-1", "instance-2", "instance-3", "", "a"] {
        assert_eq!(partition_of(key, one), 0);
    }
}

#[test]
fn the_answer_is_always_a_partition_that_exists() {
    for n in 1..=16usize {
        let count = partitions(n);
        for key in ["instance-1", "instance-2", "instance-3", "", "a", "order-1"] {
            assert!(
                partition_of(key, count) < n,
                "key {key:?} with {n} partitions must name one of them"
            );
        }
    }
}
