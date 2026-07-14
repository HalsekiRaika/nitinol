use bytes::Bytes;
use nitinol_eventsource::codec::ErasedCodec;
use nitinol_eventsource::error::CodecError;
use nitinol_eventsource::{Event, SystemEvent, SystemEventDecodeError};
use nitinol_persistence::EventType;

use crate::outbox::{is_outbox_event_type, OutboxEvent};
use crate::scheduler::{is_schedule_event_type, ScheduleEvent};

pub(crate) enum SagaPersisted<E> {
    Domain(E),
    Outbox(OutboxEvent),
    Schedule(ScheduleEvent),
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum SagaPersistedDecodeError {
    #[error("outbox marker decode failed: {0}")]
    Outbox(#[from] SystemEventDecodeError),
    #[error("schedule marker decode failed: {0}")]
    Schedule(SystemEventDecodeError),
    #[error("domain event decode failed: {0}")]
    Domain(#[from] CodecError),
}

impl<E: Event> SagaPersisted<E> {
    pub(crate) fn variant(&self) -> EventType {
        match self {
            SagaPersisted::Domain(event) => event.variant(),
            SagaPersisted::Outbox(marker) => marker.variant(),
            SagaPersisted::Schedule(marker) => marker.variant(),
        }
    }

    pub(crate) fn encode(&self, codec: &dyn ErasedCodec<E>) -> Result<Bytes, CodecError> {
        match self {
            SagaPersisted::Domain(event) => codec.encode(event),
            SagaPersisted::Outbox(marker) => Ok(marker.encode()),
            SagaPersisted::Schedule(marker) => Ok(marker.encode()),
        }
    }

    pub(crate) fn classify(
        event_type: EventType,
        payload: &[u8],
        codec: &dyn ErasedCodec<E>,
    ) -> Result<Self, SagaPersistedDecodeError> {
        // Classification order: outbox → schedule → domain.  The two framework
        // families (`nitinol.saga.outbox`, `nitinol.saga.schedule`) are
        // siblings, so a user event type can match neither.
        if is_outbox_event_type(event_type) {
            Ok(SagaPersisted::Outbox(OutboxEvent::decode(payload)?))
        } else if is_schedule_event_type(event_type) {
            ScheduleEvent::decode(payload)
                .map(SagaPersisted::Schedule)
                .map_err(SagaPersistedDecodeError::Schedule)
        } else {
            Ok(SagaPersisted::Domain(codec.decode(payload)?))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nitinol_eventsource::codec::Codec;
    use nitinol_persistence::{Family, TypeName};
    use serde::{Deserialize, Serialize};

    use crate::outbox::OutboxAppender;

    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    struct DomainEvt {
        note: String,
    }

    impl Event for DomainEvt {
        const EVENT_TYPE: EventType =
            EventType::new(Family::new("persisted_unit_test"), TypeName::new("Domain"));
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

    fn codec() -> impl ErasedCodec<DomainEvt> {
        JsonCodec
    }

    #[test]
    fn non_outbox_path_classifies_as_domain_and_round_trips() {
        let codec = codec();
        let event = DomainEvt {
            note: "hello".to_owned(),
        };
        let payload = <JsonCodec as Codec<DomainEvt>>::encode(&event).expect("encode");

        match SagaPersisted::classify(DomainEvt::EVENT_TYPE, &payload, &codec) {
            Ok(SagaPersisted::Domain(decoded)) => assert_eq!(decoded, event),
            _ => panic!("a non-outbox path must classify as Domain"),
        }
    }

    #[test]
    fn outbox_path_classifies_as_outbox_never_domain() {
        let codec = codec();
        let wire = OutboxAppender::build_tell_requested(1, 7, None, jiff::Timestamp::UNIX_EPOCH);

        match SagaPersisted::<DomainEvt>::classify(wire.event_type, &wire.payload, &codec) {
            Ok(SagaPersisted::Outbox(OutboxEvent::TellRequested(m))) => assert_eq!(m.tell_id, 7),
            _ => panic!("an outbox path must classify as Outbox"),
        }
    }

    #[test]
    fn encode_is_transparent_for_domain_events() {
        let codec = codec();
        let event = DomainEvt {
            note: "raw".to_owned(),
        };
        let via_envelope = SagaPersisted::Domain(event.clone())
            .encode(&codec)
            .expect("envelope encode");
        let direct = <JsonCodec as Codec<DomainEvt>>::encode(&event).expect("direct encode");

        assert_eq!(
            via_envelope, direct,
            "the envelope must return the inner codec bytes verbatim (no double-encode)"
        );
        assert_eq!(
            SagaPersisted::Domain(event).variant(),
            DomainEvt::EVENT_TYPE,
            "the envelope must expose the inner event's own wire identity"
        );
    }

    #[test]
    fn undecodable_domain_payload_surfaces_a_domain_decode_error() {
        let codec = codec();
        match SagaPersisted::<DomainEvt>::classify(DomainEvt::EVENT_TYPE, b"not json", &codec) {
            Err(SagaPersistedDecodeError::Domain(_)) => {}
            _ => panic!("a bad domain payload must surface a Domain decode error"),
        }
    }

    #[test]
    fn undecodable_outbox_payload_surfaces_an_outbox_decode_error() {
        let codec = codec();
        let outbox_type =
            OutboxAppender::build_tell_requested(1, 1, None, jiff::Timestamp::UNIX_EPOCH)
                .event_type;
        match SagaPersisted::<DomainEvt>::classify(outbox_type, &[], &codec) {
            Err(SagaPersistedDecodeError::Outbox(_)) => {}
            _ => panic!("an outbox marker with no oneof kind must surface an Outbox decode error"),
        }
    }
}
