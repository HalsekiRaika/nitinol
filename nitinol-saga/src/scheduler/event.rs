//! `ScheduleEvent` — the `SystemEvent` persisted for each timer transition
//! (Issue #50, E-30).
//!
//! Modelled one-for-one on the `OutboxEvent` canonical example: a reserved
//! type-level [`EventType`] (`nitinol.saga.schedule`, variant `None`) whose
//! per-arm `variant()` writes `scheduled` / `cancelled` / `fired` on the wire,
//! with a prost `oneof` codec that owns its own serialization.

use std::borrow::Borrow;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use nitinol_eventsource::{appending_system_event, SystemEvent, SystemEventDecodeError};
use nitinol_persistence::store::EventStore;
use nitinol_persistence::{EventType, Family, TypeName, Variant};

use crate::id::SagaId;
use crate::scheduler::token::{ScheduleToken, TimerName};

mod proto {
    include!(concat!(env!("OUT_DIR"), "/nitinol.saga.schedule.rs"));
}

use self::proto::schedule_marker::Kind;
use self::proto::{Cancelled, Fired, ScheduleMarker, Scheduled};

/// Reserved type-level identity shared by every schedule marker.
pub(crate) const SCHEDULE_MARKER: EventType =
    EventType::new(Family::new("nitinol.saga"), TypeName::new("schedule"));

#[derive(Debug, thiserror::Error)]
#[error("schedule marker payload carried no `oneof` kind")]
struct MissingScheduleKind;

/// A framework-managed schedule transition persisted on the saga's own stream.
pub(crate) enum ScheduleEvent {
    Scheduled {
        token: ScheduleToken,
        after: Duration,
        payload: Bytes,
        scheduled_at_unix_millis: i64,
    },
    Cancelled {
        token: ScheduleToken,
        cancelled_at_unix_millis: i64,
    },
    Fired {
        token: ScheduleToken,
        fired_at_unix_millis: i64,
    },
}

impl SystemEvent for ScheduleEvent {
    const EVENT_TYPE: EventType = SCHEDULE_MARKER;

    fn variant(&self) -> EventType {
        let variant = match self {
            ScheduleEvent::Scheduled { .. } => Variant::new("scheduled"),
            ScheduleEvent::Cancelled { .. } => Variant::new("cancelled"),
            ScheduleEvent::Fired { .. } => Variant::new("fired"),
        };
        EventType::with_variant(SCHEDULE_MARKER.family(), SCHEDULE_MARKER.type_name(), variant)
    }

    fn encode(&self) -> Bytes {
        let kind = match self {
            ScheduleEvent::Scheduled {
                token,
                after,
                payload,
                scheduled_at_unix_millis,
            } => Kind::Scheduled(Scheduled {
                saga_id: token.saga_id.as_str().to_owned(),
                name: token.name.as_str().to_owned(),
                // Lossless nanosecond representation; saturates to u64::MAX for
                // durations longer than ~584 years (not a realistic timer).
                after_nanos: u64::try_from(after.as_nanos()).unwrap_or(u64::MAX),
                payload: payload.to_vec(),
                scheduled_at_unix_millis: *scheduled_at_unix_millis,
            }),
            ScheduleEvent::Cancelled {
                token,
                cancelled_at_unix_millis,
            } => Kind::Cancelled(Cancelled {
                saga_id: token.saga_id.as_str().to_owned(),
                name: token.name.as_str().to_owned(),
                cancelled_at_unix_millis: *cancelled_at_unix_millis,
            }),
            ScheduleEvent::Fired {
                token,
                fired_at_unix_millis,
            } => Kind::Fired(Fired {
                saga_id: token.saga_id.as_str().to_owned(),
                name: token.name.as_str().to_owned(),
                fired_at_unix_millis: *fired_at_unix_millis,
            }),
        };
        let marker = ScheduleMarker { kind: Some(kind) };
        Bytes::from(prost::Message::encode_to_vec(&marker))
    }

    fn decode(payload: &[u8]) -> Result<Self, SystemEventDecodeError> {
        let marker = <ScheduleMarker as prost::Message>::decode(payload)
            .map_err(SystemEventDecodeError::new)?;
        match marker.kind {
            Some(Kind::Scheduled(m)) => Ok(ScheduleEvent::Scheduled {
                token: token_of(m.saga_id, m.name),
                after: Duration::from_nanos(m.after_nanos),
                payload: Bytes::from(m.payload),
                scheduled_at_unix_millis: m.scheduled_at_unix_millis,
            }),
            Some(Kind::Cancelled(m)) => Ok(ScheduleEvent::Cancelled {
                token: token_of(m.saga_id, m.name),
                cancelled_at_unix_millis: m.cancelled_at_unix_millis,
            }),
            Some(Kind::Fired(m)) => Ok(ScheduleEvent::Fired {
                token: token_of(m.saga_id, m.name),
                fired_at_unix_millis: m.fired_at_unix_millis,
            }),
            None => Err(SystemEventDecodeError::new(MissingScheduleKind)),
        }
    }
}

fn token_of(saga_id: String, name: String) -> ScheduleToken {
    ScheduleToken {
        saga_id: SagaId::new(saga_id),
        name: TimerName::new(name),
    }
}

/// Type-level classification: does `event_type` belong to the schedule marker
/// family (`nitinol.saga.schedule` and its per-arm variants)?
///
/// `nitinol.saga.schedule` is a sibling of `nitinol.saga.outbox`, so the two
/// classifications never overlap on segment boundaries.
pub(crate) fn is_schedule_event_type(event_type: EventType) -> bool {
    event_type.to_path().is_within(&SCHEDULE_MARKER.to_path())
}

/// Append a single schedule marker at `*sequence + 1`, advancing `*sequence`
/// only on success.  Mirrors the terminal-append discipline used for outbox
/// markers (`OutboxAppender::append_terminal`): a failed store append leaves
/// the caller's sequence untouched so the next attempt does not skip numbers.
pub(crate) async fn append_schedule_marker(
    store: &Arc<dyn EventStore>,
    saga_id: &SagaId,
    sequence: &mut u64,
    event: ScheduleEvent,
    occurred_at: jiff::Timestamp,
) -> bool {
    let candidate = *sequence + 1;
    let appending = appending_system_event(candidate, &event, occurred_at);
    if let Err(e) = store.append(saga_id.borrow(), vec![appending]).await {
        tracing::warn!(error = %e, "saga schedule marker append failed");
        return false;
    }
    *sequence = candidate;
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token() -> ScheduleToken {
        ScheduleToken {
            saga_id: SagaId::new("order-1"),
            name: TimerName::new("reminder"),
        }
    }

    #[test]
    fn schedule_event_reserves_a_single_type_level_event_type() {
        assert_eq!(ScheduleEvent::EVENT_TYPE, SCHEDULE_MARKER);
        assert_eq!(SCHEDULE_MARKER.to_string(), "nitinol.saga.schedule");
        assert!(
            SCHEDULE_MARKER.variant().is_none(),
            "the reserved schedule type is type-level (variant None)"
        );
    }

    #[test]
    fn each_arm_writes_its_per_arm_variant_on_the_wire() {
        let cases = [
            (
                ScheduleEvent::Scheduled {
                    token: token(),
                    after: Duration::from_secs(1),
                    payload: Bytes::from_static(b"p"),
                    scheduled_at_unix_millis: 5,
                },
                "scheduled",
            ),
            (
                ScheduleEvent::Cancelled {
                    token: token(),
                    cancelled_at_unix_millis: 6,
                },
                "cancelled",
            ),
            (
                ScheduleEvent::Fired {
                    token: token(),
                    fired_at_unix_millis: 7,
                },
                "fired",
            ),
        ];
        for (event, expected) in cases {
            assert_eq!(event.variant().variant(), Some(Variant::new(expected)));
            assert_eq!(event.variant().type_key(), SCHEDULE_MARKER.type_key());
        }
    }

    #[test]
    fn scheduled_round_trips_through_the_enum_codec() {
        let event = ScheduleEvent::Scheduled {
            token: token(),
            after: Duration::from_millis(1_500),
            payload: Bytes::from_static(b"hello"),
            scheduled_at_unix_millis: 1_700_000_000_000,
        };
        match ScheduleEvent::decode(&event.encode()) {
            Ok(ScheduleEvent::Scheduled {
                token: t,
                after,
                payload,
                scheduled_at_unix_millis,
            }) => {
                assert_eq!(t, token());
                assert_eq!(after, Duration::from_millis(1_500));
                assert_eq!(payload, Bytes::from_static(b"hello"));
                assert_eq!(scheduled_at_unix_millis, 1_700_000_000_000);
            }
            _ => panic!("expected decoded Scheduled"),
        }
    }

    // ARCH-001 regression: sub-millisecond and large Duration must survive the
    // encode → decode round trip without truncation.

    #[test]
    fn sub_millisecond_duration_round_trips_losslessly() {
        // 500 µs is lost entirely when stored as millis (truncates to 0).
        // With nanosecond storage it must survive the round trip exactly.
        let sub_ms = Duration::from_micros(500);
        let event = ScheduleEvent::Scheduled {
            token: token(),
            after: sub_ms,
            payload: Bytes::from_static(b""),
            scheduled_at_unix_millis: 0,
        };
        match ScheduleEvent::decode(&event.encode()) {
            Ok(ScheduleEvent::Scheduled { after, .. }) => {
                assert_eq!(
                    after, sub_ms,
                    "500 µs must survive encode→decode without truncation to 0"
                );
            }
            _ => panic!("expected decoded Scheduled"),
        }
    }

    #[test]
    fn large_duration_round_trips_without_unchecked_cast_truncation() {
        // A duration of ~100 years stored as nanos fits in u64 (u64::MAX nanos ≈ 584 years).
        // With millis storage this would require u64::MAX ms ≈ 584 million years — fine,
        // but the cast `u128 as u64` is unchecked and panics in debug mode on overflow.
        // Verify that both the 100-year case and a value just at the u64 nanosecond limit
        // round-trip without overflow.
        let one_hundred_years =
            Duration::from_secs(100 * 365 * 24 * 3600);
        let event = ScheduleEvent::Scheduled {
            token: token(),
            after: one_hundred_years,
            payload: Bytes::from_static(b""),
            scheduled_at_unix_millis: 0,
        };
        match ScheduleEvent::decode(&event.encode()) {
            Ok(ScheduleEvent::Scheduled { after, .. }) => {
                assert_eq!(
                    after, one_hundred_years,
                    "100-year duration must survive encode→decode"
                );
            }
            _ => panic!("expected decoded Scheduled"),
        }
    }

    #[test]
    fn cancelled_and_fired_are_discriminated_by_oneof_arm() {
        let cancelled = ScheduleEvent::Cancelled {
            token: token(),
            cancelled_at_unix_millis: 1,
        };
        let fired = ScheduleEvent::Fired {
            token: token(),
            fired_at_unix_millis: 1,
        };
        assert!(matches!(
            ScheduleEvent::decode(&cancelled.encode()),
            Ok(ScheduleEvent::Cancelled { .. })
        ));
        assert!(matches!(
            ScheduleEvent::decode(&fired.encode()),
            Ok(ScheduleEvent::Fired { .. })
        ));
    }

    #[test]
    fn is_schedule_event_type_accepts_reserved_type_and_variants_but_not_outbox() {
        assert!(is_schedule_event_type(SCHEDULE_MARKER));
        assert!(is_schedule_event_type(EventType::with_variant(
            SCHEDULE_MARKER.family(),
            SCHEDULE_MARKER.type_name(),
            Variant::new("scheduled"),
        )));
        // Sibling outbox family must not be classified as a schedule event.
        assert!(!is_schedule_event_type(EventType::new(
            Family::new("nitinol.saga"),
            TypeName::new("outbox"),
        )));
    }

    #[test]
    fn decode_reports_error_for_payload_missing_oneof_kind() {
        assert!(ScheduleEvent::decode(&[]).is_err());
    }
}
