//! The instant an event occurred at, crossing into a `TIMESTAMPTZ` column and
//! back.
//!
//! A `TIMESTAMPTZ` holds whole microseconds. An instant carrying a finer
//! component therefore cannot be stored as it stands, and what is written is
//! that instant truncated towards the epoch. Nothing here reaches for the
//! clock: the value stored is the one the caller supplied, so a load hands back
//! when the event happened rather than when its row was written.
//!
//! The crossing is a wrapper carrying `sqlx`'s own conversion traits rather
//! than a pair of functions called by hand, because a bind or a column
//! dispatches on the type of the value and has no way to name a function to
//! call instead. `jiff-sqlx` publishes a wrapper of exactly this shape, but it
//! is written against `sqlx` 0.8 while this crate links 0.9; a release of it
//! for 0.9 is what would take the place of the wrapper below.
//!
//! The wrapper's own crossing to and from the column is infallible — it holds
//! the column's representation directly and hands it to `sqlx` unchanged. What
//! can fail is the crossing between that representation and a [`Timestamp`],
//! which is why that direction is a `TryFrom` on the wrapper rather than part
//! of `Encode`/`Decode`: the failure it can raise is this store's own to own,
//! not `sqlx`'s.

use jiff::Timestamp;
use sqlx::encode::IsNull;
use sqlx::error::BoxDynError;
use sqlx::postgres::{PgArgumentBuffer, PgHasArrayType, PgTypeInfo, PgValueRef};
use sqlx::{Decode, Encode, Postgres, Type};
use time::OffsetDateTime;

const NANOSECONDS_PER_MICROSECOND: i128 = 1_000;

/// The instant an event occurred at, as its column carries it.
#[derive(Clone, Copy)]
pub(super) struct Instant(OffsetDateTime);

/// An instant lies outside what the other side of the column can hold.
///
/// The crossing between an instant and its column representation raises this,
/// in [`Instant`]'s own `TryFrom` conversions. The store's common failure type
/// carries it onward by name, so a caller can look for it specifically rather
/// than unwrapping a wrapper of this crate's own first.
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
        Ok(Self(<OffsetDateTime as Decode<Postgres>>::decode(value)?))
    }
}

#[cfg(test)]
mod tests {
    use jiff::Timestamp;
    use nitinol_persistence::error::{AppendError, LoadError};
    use time::OffsetDateTime;

    use super::super::failure::Failure;
    use super::{Instant, InstantOutOfRange};

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
