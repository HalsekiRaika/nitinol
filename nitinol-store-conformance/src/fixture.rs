use std::sync::Arc;

use bytes::Bytes;
use futures_util::TryStreamExt;
use jiff::Timestamp;
use nitinol_persistence::store::EventStore;
use nitinol_persistence::{AppendingEvent, EventType, Family, LoadQuery, LoadedEvent, TypeName};

/// The event type every fact the suite appends carries.
///
/// The clauses judge numbering and visibility, never routing, so one type for
/// everything is enough — and it keeps a store that filters by event type from
/// changing what any clause reads back.
const FACT: EventType = EventType::new(Family::new("occ.conformance"), TypeName::new("Fact"));

/// One event to append, carrying `payload` so a clause can tell which write it
/// is reading back.
pub(crate) fn fact(sequence: u64, payload: &'static [u8]) -> AppendingEvent {
    AppendingEvent {
        sequence,
        event_type: FACT,
        payload: Bytes::from_static(payload),
        occurred_at: Timestamp::now(),
    }
}

/// Everything the store serves for `query`, in the order it served it.
///
/// A store that cannot answer a load at all is not one any clause can judge, so
/// neither panic here names a clause: this is the suite failing to run rather
/// than a contract being broken.
pub(crate) async fn observe(backend: &Arc<dyn EventStore>, query: LoadQuery) -> Vec<LoadedEvent> {
    let stream = backend.load(query).await.unwrap_or_else(|failure| {
        panic!("the suite cannot judge a store that fails a load outright: {failure}")
    });
    stream.try_collect().await.unwrap_or_else(|failure| {
        panic!("the suite cannot judge a store that fails part-way through a stream: {failure}")
    })
}
