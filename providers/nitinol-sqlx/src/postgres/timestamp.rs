//! The instant an event occurred at, crossing into a `TIMESTAMPTZ` column and
//! back.
//!
//! A `TIMESTAMPTZ` holds whole microseconds. An instant carrying a finer
//! component therefore cannot be stored as it stands, and what is written is
//! that instant truncated towards the epoch. Nothing here reaches for the
//! clock: the value stored is the one the caller supplied, so a load hands back
//! when the event happened rather than when its row was written.

use jiff::Timestamp;
use time::OffsetDateTime;

const NANOSECONDS_PER_MICROSECOND: i128 = 1_000;

/// An instant lies outside what the other side of the column can hold.
#[derive(Debug, thiserror::Error)]
pub(super) enum InstantOutOfRange {
    #[error("an event occurred at {microsecond}µs from the epoch, which a timestamptz column cannot hold")]
    PastColumn {
        microsecond: i64,
        #[source]
        cause: time::error::ComponentRange,
    },
    #[error("a stored instant is {nanosecond}ns from the epoch, which is no instant this store can serve")]
    PastInstant {
        nanosecond: i128,
        #[source]
        cause: jiff::Error,
    },
}

pub(super) fn to_column(occurred_at: Timestamp) -> Result<OffsetDateTime, InstantOutOfRange> {
    let microsecond = occurred_at.as_microsecond();
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(microsecond) * NANOSECONDS_PER_MICROSECOND)
        .map_err(|cause| InstantOutOfRange::PastColumn { microsecond, cause })
}

pub(super) fn from_column(stored: OffsetDateTime) -> Result<Timestamp, InstantOutOfRange> {
    let nanosecond = stored.unix_timestamp_nanos();
    Timestamp::from_nanosecond(nanosecond)
        .map_err(|cause| InstantOutOfRange::PastInstant { nanosecond, cause })
}

#[cfg(test)]
mod tests {
    use jiff::Timestamp;

    use super::{from_column, to_column};

    fn round_trip(occurred_at: Timestamp) -> Timestamp {
        let stored = to_column(occurred_at).expect("an instant within the column's range");
        from_column(stored).expect("a stored instant within the range an instant has")
    }

    #[test]
    fn an_instant_on_a_microsecond_comes_back_as_it_went_in() {
        let aligned = Timestamp::from_microsecond(1_700_000_000_123_456)
            .expect("a microsecond count inside the supported range is a timestamp");

        assert_eq!(round_trip(aligned), aligned);
    }

    #[test]
    fn a_finer_component_is_truncated_towards_the_epoch() {
        let after_epoch = Timestamp::from_nanosecond(1_700_000_000_123_456_789)
            .expect("a nanosecond count inside the supported range is a timestamp");
        let before_epoch = Timestamp::from_nanosecond(-1_234_567_891)
            .expect("a nanosecond count inside the supported range is a timestamp");

        assert_eq!(
            round_trip(after_epoch),
            Timestamp::from_microsecond(1_700_000_000_123_456)
                .expect("a microsecond count inside the supported range is a timestamp"),
            "an instant past the epoch loses the part below a microsecond"
        );
        assert_eq!(
            round_trip(before_epoch),
            Timestamp::from_microsecond(-1_234_567)
                .expect("a microsecond count inside the supported range is a timestamp"),
            "an instant before the epoch truncates towards the epoch rather than away from it"
        );
    }
}
