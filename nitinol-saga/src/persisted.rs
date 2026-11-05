use nitinol_eventsource::codec;
use nitinol_eventsource::error::CodecError;
use nitinol_eventsource::{Event, SystemEvent, SystemEventDecodeError};
use nitinol_persistence::{is_within_reserved_namespace, EventType};

use crate::consumed::ConsumedPositionEvent;
use crate::lifecycle::LifecycleEvent;
use crate::scheduler::ScheduleEvent;

/// What one row of a saga's own stream is, read back as the content its
/// declaration names.
///
/// Four kinds and no more: a domain event — the catch-all — plus the three
/// facts the framework declares for itself.
pub(crate) enum SagaPersisted<E> {
    Domain(E),
    Schedule(ScheduleEvent),
    /// The position of an upstream record this saga has consumed, committed by
    /// the append that closes out the input's interpretation — see
    /// [`crate::consumed`] for what that means when one `handle` call reaches
    /// the store as more than one atomic append.
    Consumed(ConsumedPositionEvent),
    /// A fact about the saga instance's own life — so far only that it ended.
    Lifecycle(LifecycleEvent),
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum SagaPersistedDecodeError {
    #[error("schedule fact decode failed: {0}")]
    Schedule(SystemEventDecodeError),
    #[error("consumed fact decode failed: {0}")]
    Consumed(SystemEventDecodeError),
    #[error("lifecycle fact decode failed: {0}")]
    Lifecycle(SystemEventDecodeError),
    #[error("domain event decode failed: {0}")]
    Domain(#[from] CodecError),
    /// A row inside the framework's own namespace that no declaration of this
    /// version owns — a record written by an earlier version, or by a later
    /// one.  Either way the stream cannot be read, and reporting it as a
    /// domain event would hand framework bookkeeping to the saga's codec.
    #[error(
        "`{0}` is inside the framework's reserved namespace and belongs to no \
         declaration this version owns; the stream is corrupted or was written \
         by another version"
    )]
    UnknownReservedRecord(EventType),
}

impl<E: Event> SagaPersisted<E> {
    /// Read a stored row back as the content its declaration names.
    ///
    /// The three framework declarations
    /// (`nitinol.saga.{schedule,consumed,lifecycle}`) are siblings, so a row
    /// matches at most one of them and the order between those arms carries no
    /// meaning.  Every path *outside* the reserved namespace belongs to the
    /// domain and is handed to the user's own codec untouched — that catch-all
    /// is what keeps the envelope transparent to event types this crate never
    /// declared.
    ///
    /// A path *inside* the reserved namespace that matches none of the three is
    /// refused rather than sent down the catch-all.  The judgement is about the
    /// namespace, not about a list of names that once existed: a 0.4 outbox
    /// marker and a record from a later version are the same situation, and a
    /// marker's bytes may well decode cleanly as a domain event, so a
    /// payload-only judgement would fold framework bookkeeping into the saga's
    /// state.
    pub(crate) fn classify(
        event_type: EventType,
        payload: &[u8],
    ) -> Result<Self, SagaPersistedDecodeError> {
        match event_type {
            ScheduleEvent::EVENT_TYPE => ScheduleEvent::decode(payload)
                .map(SagaPersisted::Schedule)
                .map_err(SagaPersistedDecodeError::Schedule),
            ConsumedPositionEvent::EVENT_TYPE => ConsumedPositionEvent::decode(payload)
                .map(SagaPersisted::Consumed)
                .map_err(SagaPersistedDecodeError::Consumed),
            LifecycleEvent::EVENT_TYPE => LifecycleEvent::decode(payload)
                .map(SagaPersisted::Lifecycle)
                .map_err(SagaPersistedDecodeError::Lifecycle),
            _ if is_within_reserved_namespace(event_type.family().as_str()) => {
                Err(SagaPersistedDecodeError::UnknownReservedRecord(event_type))
            }
            _ => Ok(SagaPersisted::Domain(codec::decode::<E::Codec, E>(
                payload,
            )?)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use nitinol_eventsource::codec::Codec;
    use nitinol_persistence::__private::reserved_family;
    use nitinol_persistence::{AggregateId, Family, TypeName};
    use serde::{Deserialize, Serialize};

    use crate::consumed::PositionSource;
    use crate::id::SagaId;
    use crate::scheduler::{ScheduleToken, TimerName};

    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    struct DomainEvt {
        note: String,
    }

    struct JsonCodec;

    impl<E: Serialize + for<'de> Deserialize<'de> + 'static> Codec<E> for JsonCodec {
        type Error = serde_json::Error;

        fn encode(event: &E) -> Result<Bytes, Self::Error> {
            serde_json::to_vec(event).map(Bytes::from)
        }

        fn decode(payload: &[u8]) -> Result<E, Self::Error> {
            serde_json::from_slice(payload)
        }
    }

    impl Event for DomainEvt {
        const EVENT_TYPE: EventType =
            EventType::new(Family::new("persisted_unit_test"), TypeName::new("Domain"));

        type Codec = JsonCodec;
    }

    #[test]
    fn a_domain_path_classifies_as_domain_and_round_trips() {
        let event = DomainEvt {
            note: "hello".to_owned(),
        };
        let payload = <JsonCodec as Codec<DomainEvt>>::encode(&event).expect("encode");

        match SagaPersisted::<DomainEvt>::classify(DomainEvt::EVENT_TYPE, &payload) {
            Ok(SagaPersisted::Domain(decoded)) => assert_eq!(decoded, event),
            _ => panic!("a path outside the reserved namespace must classify as Domain"),
        }
    }

    #[test]
    fn schedule_path_classifies_as_schedule_never_domain() {
        let marker = ScheduleEvent::Fired {
            token: ScheduleToken {
                saga_id: SagaId::new("saga-1"),
                name: TimerName::new("reminder"),
            },
            fired_at_unix_millis: 1_700_000_000_000,
        };

        match SagaPersisted::<DomainEvt>::classify(ScheduleEvent::EVENT_TYPE, &marker.encode()) {
            Ok(SagaPersisted::Schedule(ScheduleEvent::Fired { token, .. })) => {
                assert_eq!(token.name, TimerName::new("reminder"));
            }
            _ => panic!("a schedule path must classify as Schedule"),
        }
    }

    #[test]
    fn lifecycle_path_classifies_as_lifecycle_never_domain() {
        match SagaPersisted::<DomainEvt>::classify(
            LifecycleEvent::EVENT_TYPE,
            &LifecycleEvent::Ended.encode(),
        ) {
            Ok(SagaPersisted::Lifecycle(LifecycleEvent::Ended)) => {}
            _ => panic!("a lifecycle path must classify as Lifecycle"),
        }
    }

    /// A position is the framework's own bookkeeping, so it must never reach
    /// the user's codec — and never be handed back as a domain event for
    /// `Saga::apply` to fold into the saga's state.
    #[test]
    fn consumed_path_classifies_as_a_position_never_domain() {
        let position = ConsumedPositionEvent {
            source: AggregateId::new("order-stream"),
            source_event_type: PositionSource::of(DomainEvt::EVENT_TYPE),
            sequence: 7,
        };

        match SagaPersisted::<DomainEvt>::classify(
            ConsumedPositionEvent::EVENT_TYPE,
            &position.encode(),
        ) {
            Ok(SagaPersisted::Consumed(read)) => {
                assert_eq!(read.source.as_str(), "order-stream");
                assert_eq!(read.sequence, 7);
            }
            _ => panic!("a consumed path must classify as Consumed"),
        }
    }

    /// A reserved path no declaration owns is refused on the strength of its
    /// identity, with bytes that *would* decode as a domain event — so a
    /// catch-all that judged only the payload is ruled out here, not just in
    /// the integration test that observes the refused load.
    #[test]
    fn an_unknown_reserved_path_is_refused_rather_than_read_as_domain() {
        let released = EventType::new(reserved_family("nitinol.saga"), TypeName::new("outbox"));
        let decodes_as_domain = <JsonCodec as Codec<DomainEvt>>::encode(&DomainEvt {
            note: "disguised".to_owned(),
        })
        .expect("encode");

        match SagaPersisted::<DomainEvt>::classify(released, &decodes_as_domain) {
            Err(SagaPersistedDecodeError::UnknownReservedRecord(reported)) => {
                assert_eq!(reported, released)
            }
            _ => panic!(
                "a reserved path this version declares nothing for must be refused, \
                 however readable its bytes happen to be"
            ),
        }
    }

    /// The refusal is a judgement about the namespace rather than a list of
    /// names that once existed, so a reserved path this crate has never written
    /// is refused on the same grounds.
    #[test]
    fn an_unheard_of_reserved_path_is_refused_on_the_same_grounds() {
        let unheard_of = EventType::new(
            reserved_family("nitinol.saga"),
            TypeName::new("from_a_later_version"),
        );

        assert!(matches!(
            SagaPersisted::<DomainEvt>::classify(unheard_of, &[]),
            Err(SagaPersistedDecodeError::UnknownReservedRecord(_))
        ));
    }

    #[test]
    fn undecodable_consumed_payload_surfaces_a_consumed_decode_error() {
        match SagaPersisted::<DomainEvt>::classify(ConsumedPositionEvent::EVENT_TYPE, &[]) {
            Err(SagaPersistedDecodeError::Consumed(_)) => {}
            _ => panic!(
                "a position with no oneof kind must surface a Consumed decode error, \
                 so the load is refused instead of reading it as a position of its own"
            ),
        }
    }

    #[test]
    fn undecodable_schedule_payload_surfaces_a_schedule_decode_error() {
        match SagaPersisted::<DomainEvt>::classify(ScheduleEvent::EVENT_TYPE, &[]) {
            Err(SagaPersistedDecodeError::Schedule(_)) => {}
            _ => panic!("a schedule fact with no oneof kind must surface a Schedule decode error"),
        }
    }

    #[test]
    fn undecodable_lifecycle_payload_surfaces_a_lifecycle_decode_error() {
        match SagaPersisted::<DomainEvt>::classify(LifecycleEvent::EVENT_TYPE, &[]) {
            Err(SagaPersistedDecodeError::Lifecycle(_)) => {}
            _ => {
                panic!("a lifecycle fact with no oneof kind must surface a Lifecycle decode error")
            }
        }
    }

    #[test]
    fn undecodable_domain_payload_surfaces_a_domain_decode_error() {
        match SagaPersisted::<DomainEvt>::classify(DomainEvt::EVENT_TYPE, b"not json") {
            Err(SagaPersistedDecodeError::Domain(_)) => {}
            _ => panic!("a bad domain payload must surface a Domain decode error"),
        }
    }
}
