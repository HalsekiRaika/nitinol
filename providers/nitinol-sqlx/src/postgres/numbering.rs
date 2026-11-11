//! Where the store's `u64` numbering meets a `BIGINT` column.
//!
//! The two ranges do not coincide, and a conversion that wrapped or saturated
//! would silently change which event a sequence number names. Every crossing
//! goes through here and every one that cannot be made fails.

/// A number does not survive the crossing between the store's numbering and a
/// `BIGINT` column.
#[derive(Debug, thiserror::Error)]
pub(super) enum OutOfRange {
    #[error("{name} is {value}, which is past the largest value a BIGINT column holds")]
    PastBigint { name: &'static str, value: u64 },
    #[error("the {name} column holds {value}, and the store numbers upwards from zero")]
    Negative { name: &'static str, value: i64 },
}

pub(super) fn to_bigint(name: &'static str, value: u64) -> Result<i64, OutOfRange> {
    i64::try_from(value).map_err(|_| OutOfRange::PastBigint { name, value })
}

pub(super) fn from_bigint(name: &'static str, value: i64) -> Result<u64, OutOfRange> {
    u64::try_from(value).map_err(|_| OutOfRange::Negative { name, value })
}

#[cfg(test)]
mod tests {
    use super::{from_bigint, to_bigint, OutOfRange};

    #[test]
    fn a_number_a_bigint_column_holds_crosses_in_both_directions() {
        let largest = u64::try_from(i64::MAX).expect("i64::MAX is not negative");

        assert!(matches!(to_bigint("sequence", 0), Ok(0)));
        assert!(matches!(to_bigint("sequence", largest), Ok(i64::MAX)));
        assert!(matches!(from_bigint("sequence", 0), Ok(0)));
        assert_eq!(
            from_bigint("sequence", i64::MAX).expect("i64::MAX is a sequence a stream can reach"),
            largest
        );
    }

    #[test]
    fn a_number_past_the_column_fails_rather_than_wrapping() {
        let past = u64::try_from(i64::MAX).expect("i64::MAX is not negative") + 1;

        assert!(matches!(
            to_bigint("sequence", past),
            Err(OutOfRange::PastBigint { .. })
        ));
    }

    #[test]
    fn a_negative_column_value_is_no_sequence() {
        assert!(matches!(
            from_bigint("global_sequence", -1),
            Err(OutOfRange::Negative { .. })
        ));
    }
}
