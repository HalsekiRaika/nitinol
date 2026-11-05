//! An instant a caller handed over — when an event occurred, when a snapshot
//! was taken — crossing into a `TIMESTAMPTZ` column and back.
//!
//! A `TIMESTAMPTZ` holds whole microseconds. An instant carrying a finer
//! component therefore cannot be stored as it stands, and what is written is
//! that instant truncated towards the epoch. Nothing here reaches for the
//! clock: the value stored is the one the caller supplied, so a load hands back
//! when the thing happened rather than when its row was written.
//!
//! The crossing is a wrapper carrying `sqlx`'s own conversion traits rather
//! than a pair of functions called by hand, because a bind or a column
//! dispatches on the type of the value and has no way to name a function to
//! call instead. `jiff-sqlx` publishes a wrapper of exactly this shape, but it
//! is written against `sqlx` 0.8 while this crate links 0.9; a release of it
//! for 0.9 is what would take the place of the wrapper below.
//!
//! Writing the wrapper to the column is infallible — it holds the column's
//! representation directly and hands it to `sqlx` unchanged. What can fail is
//! the crossing between that representation and a [`Timestamp`], which is why
//! that direction is a `TryFrom` on the wrapper rather than part of
//! `Encode`/`Decode`: the failure it can raise is this store's own to own, not
//! `sqlx`'s.
//!
//! Reading is where the column reaches further than the wrapper does. Postgres'
//! range is wider than the dates the representation below holds, so a row can
//! carry a moment that cannot be read into it at all, and such a row is one the
//! store has to report: an activation answered with no snapshot, or a reader
//! served an event short, could not tell it from a row that was never there.
//! The microsecond count is therefore converted here rather than by `sqlx`'s
//! own conversion for this representation, which adds a duration to a date and
//! panics instead of failing where the sum is past the last date it holds.

use jiff::Timestamp;
use sqlx::encode::IsNull;
use sqlx::error::BoxDynError;
use sqlx::postgres::{PgArgumentBuffer, PgHasArrayType, PgTypeInfo, PgValueFormat, PgValueRef};
use sqlx::{Decode, Encode, Postgres, Type};
use time::OffsetDateTime;

const NANOSECONDS_PER_MICROSECOND: i128 = 1_000;

/// What a `TIMESTAMPTZ` counts from — 2000-01-01T00:00:00Z — as nanoseconds
/// from the unix epoch.
const POSTGRES_EPOCH_NANOSECOND: i128 = 946_684_800 * 1_000_000_000;

/// An instant a caller handed over, as its column carries it.
#[derive(Clone, Copy)]
pub(super) struct Instant(OffsetDateTime);

/// An instant lies outside what the other side of the column can hold.
///
/// The crossings between an instant and its column representation raise this,
/// in [`Instant`]'s own conversions. The store's common failure type carries it
/// onward by name, so a caller can look for it specifically rather than
/// unwrapping a wrapper of this crate's own first.
#[derive(Debug, thiserror::Error)]
pub(super) enum InstantOutOfRange {
    #[error("an instant {microsecond}µs from the epoch is one a timestamptz column cannot hold")]
    PastColumn {
        microsecond: i64,
        #[source]
        cause: time::error::ComponentRange,
    },
    /// A stored count names a moment that cannot be read into the column's
    /// representation at all.
    ///
    /// One step further out than [`PastInstant`](Self::PastInstant), which is a
    /// moment that is read and then cannot be served.
    #[error("a stored instant is {microsecond}µs from 2000-01-01, which is past every moment a timestamptz column is read into")]
    StoredPastColumn {
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

impl TryFrom<Timestamp> for Instant {
    type Error = InstantOutOfRange;

    fn try_from(occurred_at: Timestamp) -> Result<Self, Self::Error> {
        let microsecond = occurred_at.as_microsecond();
        OffsetDateTime::from_unix_timestamp_nanos(
            i128::from(microsecond) * NANOSECONDS_PER_MICROSECOND,
        )
        .map(Self)
        .map_err(|cause| InstantOutOfRange::PastColumn { microsecond, cause })
    }
}

impl TryFrom<Instant> for Timestamp {
    type Error = InstantOutOfRange;

    fn try_from(instant: Instant) -> Result<Self, Self::Error> {
        let nanosecond = instant.0.unix_timestamp_nanos();
        Timestamp::from_nanosecond(nanosecond)
            .map_err(|cause| InstantOutOfRange::PastInstant { nanosecond, cause })
    }
}

impl Type<Postgres> for Instant {
    fn type_info() -> PgTypeInfo {
        <OffsetDateTime as Type<Postgres>>::type_info()
    }

    fn compatible(ty: &PgTypeInfo) -> bool {
        <OffsetDateTime as Type<Postgres>>::compatible(ty)
    }
}

/// An append binds a whole batch's instants as one array.
impl PgHasArrayType for Instant {
    fn array_type_info() -> PgTypeInfo {
        <OffsetDateTime as PgHasArrayType>::array_type_info()
    }

    fn array_compatible(ty: &PgTypeInfo) -> bool {
        <OffsetDateTime as PgHasArrayType>::array_compatible(ty)
    }
}

impl Encode<'_, Postgres> for Instant {
    fn encode_by_ref(&self, buf: &mut PgArgumentBuffer) -> Result<IsNull, BoxDynError> {
        <OffsetDateTime as Encode<Postgres>>::encode_by_ref(&self.0, buf)
    }
}

impl<'r> Decode<'r, Postgres> for Instant {
    fn decode(value: PgValueRef<'r>) -> Result<Self, BoxDynError> {
        match value.format() {
            // The column's binary form is a microsecond count from its own
            // epoch, which is the one form a prepared statement is served in.
            PgValueFormat::Binary => Ok(Self(stored(<i64 as Decode<Postgres>>::decode(value)?)?)),
            // A text result carries the moment as digits, and parsing them is
            // where a moment too far out is already reported rather than
            // panicked on.
            PgValueFormat::Text => Ok(Self(<OffsetDateTime as Decode<Postgres>>::decode(value)?)),
        }
    }
}

/// The moment a stored `microsecond` count from the column's own epoch names.
///
/// The sum is taken in a width no column value can overflow, so what decides
/// the answer is the range of the representation rather than the arithmetic
/// reaching it.
fn stored(microsecond: i64) -> Result<OffsetDateTime, InstantOutOfRange> {
    OffsetDateTime::from_unix_timestamp_nanos(
        i128::from(microsecond) * NANOSECONDS_PER_MICROSECOND + POSTGRES_EPOCH_NANOSECOND,
    )
    .map_err(|cause| InstantOutOfRange::StoredPastColumn { microsecond, cause })
}

#[cfg(test)]
mod tests {
    use jiff::Timestamp;
    use nitinol_persistence::error::{AppendError, LoadError};
    use time::OffsetDateTime;

    use super::super::failure::Failure;
    use super::{stored, Instant, InstantOutOfRange};

    fn round_trip(occurred_at: Timestamp) -> Timestamp {
        let stored = Instant::try_from(occurred_at).expect("an instant within the column's range");
        Timestamp::try_from(stored).expect("a stored instant within the range an instant has")
    }

    /// A column value past every instant.
    ///
    /// The column's own type reaches further than an instant does, so a row can
    /// hold a moment this store cannot serve. `Date::MAX` is past the last
    /// instant under either of `time`'s date ranges.
    fn past_every_instant() -> OffsetDateTime {
        time::Date::MAX.midnight().assume_utc()
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

    #[test]
    fn a_column_value_past_every_instant_fails_rather_than_being_clamped() {
        assert!(
            matches!(
                Timestamp::try_from(Instant(past_every_instant())),
                Err(InstantOutOfRange::PastInstant { .. })
            ),
            "a stored value beyond every instant is refused rather than clamped to the last one \
             or read as a neighbouring instant"
        );
    }

    /// A stored count is read from the column's own epoch rather than the unix
    /// one.
    #[test]
    fn a_stored_count_is_read_from_the_epoch_a_timestamptz_counts_from() {
        let a_second_in = stored(1_000_000).expect("a second past 2000-01-01 is a moment held");

        assert_eq!(
            Timestamp::try_from(Instant(a_second_in))
                .expect("a moment a second past 2000-01-01 is an instant"),
            Timestamp::from_second(946_684_801)
                .expect("a second count inside the supported range is a timestamp"),
            "a timestamptz counts microseconds from 2000-01-01T00:00:00Z, so a stored count of \
             one second names that moment plus a second rather than one past the unix epoch"
        );
    }

    /// A stored count past every date the column is read into.
    ///
    /// Postgres' range reaches further than that representation, so such a row
    /// can exist — and the conversion `sqlx` publishes for this representation
    /// panics on it, which is why the count is converted in this module
    /// instead. A panic is raised on the caller's own task: a load would not
    /// report a failure it could act on, and nothing downstream would see the
    /// row at all.
    #[test]
    fn a_stored_count_past_every_date_the_column_is_read_into_fails_rather_than_panicking() {
        assert!(
            matches!(
                stored(i64::MAX),
                Err(InstantOutOfRange::StoredPastColumn { .. })
            ),
            "a stored count naming a moment past the representation a column is read into is \
             refused, so the row reaches a caller as a failure rather than as a panic or as no \
             row at all"
        );
    }

    /// A value the crossing refuses, for the tests below.
    fn out_of_range() -> InstantOutOfRange {
        Timestamp::try_from(Instant(past_every_instant()))
            .expect_err("a column value past every instant fails")
    }

    #[test]
    fn an_out_of_range_instant_becomes_an_appends_backend_failure_the_caller_can_name() {
        match AppendError::from(Failure::from(out_of_range())) {
            AppendError::Backend(cause) => assert!(
                cause.downcast_ref::<InstantOutOfRange>().is_some(),
                "an append's backend failure names the instant that was out of range"
            ),
            AppendError::SequenceConflict(key) => panic!(
                "an out-of-range instant is a backend fault, not a conflict on stream {key:?}"
            ),
        }
    }

    #[test]
    fn an_out_of_range_instant_becomes_a_loads_backend_failure_the_caller_can_name() {
        match LoadError::from(Failure::from(out_of_range())) {
            LoadError::Backend(cause) => assert!(
                cause.downcast_ref::<InstantOutOfRange>().is_some(),
                "a load's backend failure names the instant that was out of range"
            ),
        }
    }
}
