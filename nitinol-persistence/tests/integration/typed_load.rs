//! Reading the rows of one event type back as that type.
//!
//! `LOAD-2`: `read(q)` is what `load` answers for `q.into_untyped()`,
//! keeping only the rows of `E` itself and reading each through `E::Codec`,
//! in the order and at the positions `load` gives them. A payload the codec
//! cannot read is an error in that row's place, and the stream goes on past
//! it.
//!
//! The type is part of the query: `LoadQuery::of::<E>()` is the only way to a
//! `LoadQuery<Typed<E>>`, and `into_untyped` hands the store the same
//! conditions with `E`'s path as the prefix, whatever the prefix field was
//! left holding.
//!
//! ```gherkin
//!   Scenario: the rows of one type
//!     Given rows of `Wanted`, of a sibling type, and of a declaration whose
//!          family is `Wanted`'s whole path
//!     When `Wanted` is read
//!     Then only the `Wanted` rows come back, in ascending global order, at
//!          the positions `load` serves them at
//!
//!   Scenario: the query the store receives
//!     Given a query built for `Wanted`
//!     Then its erased form names `Wanted`'s path and every other condition
//!          the query was given, and nothing else
//!     When the typed query's prefix field is overwritten
//!     Then its erased form still names `Wanted`'s path, and the read still
//!          serves `Wanted`'s rows
//!
//!   Scenario: a deeper declaration
//!     Given a declaration whose family is `Wanted`'s whole path
//!     Then the load of the erased query serves its rows, and the read leaves
//!          them out
//!
//!   Scenario: the rest of the query
//!     Then the stream, the global position, the stream sequence and the limit
//!          the query names still apply, to `Wanted`'s rows
//!
//!   Scenario: a payload the codec cannot read
//!     Then that row is a decode error naming the row, and the rows after it
//!          are still read
//! ```
//!
//! Four handles reach a store, and the read is reachable from each: the
//! backend's own type, `&dyn EventStore`, the `Journal` the framework
//! distributes, and the in-memory `InMemoryStore` that holds one.

use std::collections::HashMap;
use std::fmt;

use bytes::Bytes;
use futures_util::{StreamExt, TryStreamExt};
use jiff::Timestamp;
use nitinol_contract::{Codec, Event};
use nitinol_persistence::error::LoadError;
use nitinol_persistence::provider::Store;
use nitinol_persistence::store::{
    EventStore, InMemoryEventStore, InMemoryStore, Journal, TypedEvent, TypedLoad,
};
use nitinol_persistence::{
    AppendingEvent, EventType, Family, LoadQuery, LoadedEvent, MaterializedPath, TypeName, Typed,
    Untyped,
};

const FAMILY: Family = Family::new("typed.load");
const WANTED: EventType = EventType::new(FAMILY, TypeName::new("Wanted"));
const SIBLING: EventType = EventType::new(FAMILY, TypeName::new("Sibling"));
/// A declaration whose family is `WANTED`'s whole path, so a prefix naming
/// `WANTED` reaches its rows too.
const DEEPER: EventType = EventType::new(Family::new("typed.load.Wanted"), TypeName::new("Deeper"));

#[derive(Debug, Clone, PartialEq)]
struct Wanted(String);

impl Event for Wanted {
    const EVENT_TYPE: EventType = WANTED;

    type Codec = Tagged;
}

/// A format that reads a payload carrying its tag and refuses anything else.
struct Tagged;

const TAG: &[u8] = b"wanted:";

#[derive(Debug)]
struct Untagged;

impl fmt::Display for Untagged {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the payload does not carry the `wanted:` tag")
    }
}

impl std::error::Error for Untagged {}

impl Codec<Wanted> for Tagged {
    type Error = Untagged;

    fn encode(value: &Wanted) -> Result<Bytes, Self::Error> {
        Ok(Bytes::from([TAG, value.0.as_bytes()].concat()))
    }

    fn decode(payload: &[u8]) -> Result<Wanted, Self::Error> {
        let text = payload.strip_prefix(TAG).ok_or(Untagged)?;
        let text = std::str::from_utf8(text).map_err(|_| Untagged)?;
        Ok(Wanted(text.to_owned()))
    }
}

/// The instant the row appended `nth` carries, distinct for every row so a
/// read that moved one row's time onto another is seen.
fn instant(nth: usize) -> Timestamp {
    let offset = i64::try_from(nth).expect("a fixture index fits in i64");
    Timestamp::from_second(1_700_000_000 + offset)
        .expect("a second count inside the supported range is a timestamp")
}

/// Appends each row to its stream on its own, numbering every stream from one,
/// and returns the global position each row was assigned, in fixture order.
///
/// Every payload carries the tag, the rows of the other declarations included:
/// a read that kept a row of another declaration then shows it as one more
/// `Wanted` value rather than as a decode error.
async fn seeded(rows: &[(&str, EventType, &'static [u8])]) -> (InMemoryEventStore, Vec<u64>) {
    let store = InMemoryEventStore::default();
    let mut next_sequence: HashMap<&str, u64> = HashMap::new();
    let mut positions = Vec::new();
    for (nth, (stream, event_type, payload)) in rows.iter().copied().enumerate() {
        let sequence = next_sequence.entry(stream).or_insert(0);
        *sequence += 1;
        let outcome = store
            .append(
                stream,
                vec![AppendingEvent {
                    sequence: *sequence,
                    event_type,
                    payload: Bytes::from_static(payload),
                    occurred_at: instant(nth),
                }],
            )
            .await
            .expect("appending a fixture row succeeds");
        positions.push(
            *outcome
                .assigned_sequences
                .first()
                .expect("a one-event append is assigned one global position"),
        );
    }
    (store, positions)
}

/// One row of each kind interleaved across two streams, so no type owns a
/// stream or a contiguous stretch of the global order.
async fn mixed() -> (InMemoryEventStore, Vec<u64>) {
    seeded(&[
        ("stream-a", WANTED, b"wanted:first"),
        ("stream-b", SIBLING, b"wanted:sibling"),
        ("stream-a", DEEPER, b"wanted:deeper"),
        ("stream-b", WANTED, b"wanted:second"),
        ("stream-a", SIBLING, b"wanted:sibling"),
        ("stream-a", WANTED, b"wanted:third"),
    ])
    .await
}

/// Every item a typed read yields, in order, errors included.
async fn items<S: TypedLoad + ?Sized>(
    store: &S,
    query: LoadQuery<Typed<Wanted>>,
) -> Vec<Result<TypedEvent<Wanted>, LoadError>> {
    store
        .read(query)
        .await
        .expect("a typed read over the reference implementation is served")
        .collect()
        .await
}

/// The values a typed read yields, which the caller requires to be all readable.
async fn values<S: TypedLoad + ?Sized>(store: &S, query: LoadQuery<Typed<Wanted>>) -> Vec<String> {
    items(store, query)
        .await
        .into_iter()
        .enumerate()
        .map(|(nth, item)| match item {
            Ok(typed) => typed.event.0,
            Err(failure) => panic!("item {nth} of a typed read failed: {failure}"),
        })
        .collect()
}

fn path(text: &str) -> MaterializedPath {
    text.parse()
        .expect("a fixture prefix parses as a materialized path")
}

/// Every condition the query a store receives carries, in a form that compares.
///
/// `LoadQuery` does not derive `PartialEq`, so the comparison is spelled out
/// here over its public fields rather than by widening what the crate derives.
#[derive(Debug, PartialEq)]
struct Conditions {
    stream_key: Option<String>,
    event_type_prefix: Option<MaterializedPath>,
    from_global_sequence: Option<u64>,
    from_stream_sequence: Option<u64>,
    limit: Option<usize>,
}

fn conditions(query: LoadQuery<Untyped>) -> Conditions {
    Conditions {
        stream_key: query.stream_key,
        event_type_prefix: query.event_type_prefix,
        from_global_sequence: query.from_global_sequence,
        from_stream_sequence: query.from_stream_sequence,
        limit: query.limit,
    }
}

#[test]
fn load_2_a_query_of_a_type_names_its_path_and_no_other_condition() {
    assert_eq!(
        conditions(LoadQuery::of::<Wanted>().into_untyped()),
        Conditions {
            stream_key: None,
            event_type_prefix: Some(WANTED.to_path()),
            from_global_sequence: None,
            from_stream_sequence: None,
            limit: None,
        },
        "a query built for a type hands the store that type's path as its \
         prefix, and no condition beside it"
    );
}

#[test]
fn load_2_erasing_keeps_every_condition_the_typed_query_was_given() {
    let query = LoadQuery::of::<Wanted>()
        .with_stream("stream-a")
        .with_from_global(7)
        .with_from_stream_sequence(3)
        .with_limit(5);

    assert_eq!(
        conditions(query.into_untyped()),
        Conditions {
            stream_key: Some("stream-a".to_owned()),
            event_type_prefix: Some(WANTED.to_path()),
            from_global_sequence: Some(7),
            from_stream_sequence: Some(3),
            limit: Some(5),
        },
        "the query the store receives carries the stream, the global position, \
         the stream sequence and the limit the typed query was given, each in \
         its own place, beside the type's path"
    );
}

#[test]
fn load_2_converting_with_from_gives_the_query_into_untyped_gives() {
    let query = LoadQuery::of::<Wanted>()
        .with_stream("stream-a")
        .with_limit(5);

    assert_eq!(
        conditions(query.clone().into()),
        conditions(query.into_untyped()),
        "`From` brings a typed query down to the same query the store receives \
         from `into_untyped`: the stream, the type's path, both positions and \
         the limit"
    );
}

#[test]
fn load_2_erasing_puts_the_types_own_path_back_whatever_the_prefix_field_holds() {
    for (named, overwritten) in [
        ("no prefix", None),
        ("an ancestor family", Some(path("typed"))),
        ("a sibling declaration", Some(SIBLING.to_path())),
        ("a deeper declaration", Some(DEEPER.to_path())),
        ("an unrelated family", Some(path("elsewhere"))),
    ] {
        let mut query = LoadQuery::of::<Wanted>().with_stream("stream-a");
        query.event_type_prefix = overwritten;

        assert_eq!(
            conditions(query.into_untyped()),
            Conditions {
                stream_key: Some("stream-a".to_owned()),
                event_type_prefix: Some(WANTED.to_path()),
                from_global_sequence: None,
                from_stream_sequence: None,
                limit: None,
            },
            "a typed query whose prefix field was overwritten with {named} \
             still hands the store the type's own path, and keeps the stream"
        );
    }
}

#[tokio::test]
async fn load_2_a_read_serves_its_own_rows_whatever_the_prefix_field_holds() {
    let (store, _) = mixed().await;
    let mut query = LoadQuery::of::<Wanted>();
    query.event_type_prefix = Some(path("elsewhere"));

    assert_eq!(
        values(&store, query).await,
        vec!["first", "second", "third"],
        "the store is handed the erased query, so a prefix field overwritten \
         with a path no row is within neither empties the read nor reaches \
         the store"
    );
}

#[tokio::test]
async fn load_2_leaves_out_the_rows_of_a_deeper_declaration_its_path_reaches() {
    let (store, _) = mixed().await;
    let loaded: Vec<LoadedEvent> = store
        .load(LoadQuery::of::<Wanted>().into_untyped())
        .await
        .expect("loading from the reference implementation succeeds")
        .try_collect()
        .await
        .expect("collecting a loaded stream succeeds");
    assert!(
        loaded.iter().any(|row| row.event_type == DEEPER),
        "the load of the erased query serves the deeper declaration's row, or \
         the read is not what leaves it out"
    );

    let read = values(&store, LoadQuery::of::<Wanted>()).await;

    assert_eq!(
        read,
        vec!["first", "second", "third"],
        "the read compares each row's type with its own: the deeper \
         declaration's row the store served is not read as `Wanted`"
    );
}

#[tokio::test]
async fn load_2_reads_the_rows_of_its_own_type_and_no_other() {
    assert!(
        DEEPER.to_path().is_within(&WANTED.to_path()),
        "the fixture needs a declaration a prefix naming `Wanted` reaches, or \
         the exact comparison is not exercised"
    );
    let (store, _) = mixed().await;

    assert_eq!(
        values(&store, LoadQuery::of::<Wanted>()).await,
        vec!["first", "second", "third"],
        "a read of one type keeps that type's rows alone: a sibling declaration \
         is not within its path, and a deeper declaration is within it but is \
         not it"
    );
}

#[tokio::test]
async fn load_2_keeps_the_order_and_positions_load_serves_its_rows_at() {
    let (store, positions) = mixed().await;
    let loaded: Vec<LoadedEvent> = store
        .load(LoadQuery::of::<Wanted>().into_untyped())
        .await
        .expect("loading from the reference implementation succeeds")
        .try_collect()
        .await
        .expect("collecting a loaded stream succeeds");
    let expected: Vec<(String, u64, u64, Timestamp)> = loaded
        .into_iter()
        .filter(|row| row.event_type == WANTED)
        .map(|row| {
            (
                row.stream_key,
                row.sequence,
                row.global_sequence,
                row.occurred_at,
            )
        })
        .collect();

    let read: Vec<(String, u64, u64, Timestamp)> = items(&store, LoadQuery::of::<Wanted>())
        .await
        .into_iter()
        .map(|item| {
            let typed = item.expect("every `Wanted` row of the fixture is readable");
            (
                typed.stream_key,
                typed.sequence,
                typed.global_sequence,
                typed.occurred_at,
            )
        })
        .collect();

    assert_eq!(
        read, expected,
        "each typed row carries the stream, the stream sequence, the global \
         position and the instant of the row it was read from, in the order \
         `load` serves those rows"
    );
    assert_eq!(
        read.iter()
            .map(|(_, _, global_sequence, _)| *global_sequence)
            .collect::<Vec<_>>(),
        vec![positions[0], positions[3], positions[5]],
        "the rows come back in ascending global order"
    );
}

#[tokio::test]
async fn load_2_is_a_conjunction_with_the_rest_of_the_query() {
    // Rows of a sibling type sit between the `Wanted` ones on the queried
    // stream, so a limit the store counted over rows of every type comes up
    // short.
    let (store, positions) = seeded(&[
        ("stream-a", WANTED, b"wanted:a1"),
        ("stream-b", WANTED, b"wanted:b1"),
        ("stream-a", SIBLING, b"wanted:sibling"),
        ("stream-a", WANTED, b"wanted:a2"),
        ("stream-a", SIBLING, b"wanted:sibling"),
        ("stream-a", WANTED, b"wanted:a3"),
        ("stream-a", WANTED, b"wanted:a4"),
    ])
    .await;

    assert_eq!(
        values(
            &store,
            LoadQuery::of::<Wanted>()
                .with_stream("stream-a")
                .with_from_global(positions[1])
                .with_limit(2),
        )
        .await,
        vec!["a2", "a3"],
        "the stream, the global position and the limit all apply, and the \
         limit counts rows of the read type: the other stream's row at the \
         position, the row below it and the sibling rows between are not read"
    );
    assert_eq!(
        values(
            &store,
            LoadQuery::of::<Wanted>()
                .with_stream("stream-a")
                .with_from_stream_sequence(4),
        )
        .await,
        vec!["a3", "a4"],
        "a stream sequence applies to the stream's own numbering, which the \
         rows of other types take part in"
    );
}

#[tokio::test]
async fn load_2_a_payload_its_codec_cannot_read_is_an_error_in_place_and_the_read_goes_on() {
    let (store, positions) = seeded(&[
        ("stream-a", WANTED, b"wanted:before"),
        ("stream-a", WANTED, b"garbled"),
        ("stream-a", WANTED, b"wanted:after"),
    ])
    .await;

    let read = items(&store, LoadQuery::of::<Wanted>()).await;

    assert_eq!(
        read.len(),
        3,
        "an unreadable row neither ends the stream nor is skipped: every row \
         yields one item, got {read:?}"
    );
    match &read[0] {
        Ok(typed) => assert_eq!(typed.event, Wanted("before".to_owned())),
        Err(failure) => panic!("the row before the unreadable one is read, got {failure}"),
    }
    match &read[1] {
        Err(LoadError::Decode {
            event_type,
            stream_key,
            global_sequence,
            source,
        }) => {
            assert_eq!(
                (*event_type, stream_key.as_str(), *global_sequence),
                (WANTED, "stream-a", positions[1]),
                "the decode error names the type, the stream and the global \
                 position of the row it could not read"
            );
            assert!(
                source.downcast_ref::<Untagged>().is_some(),
                "the decode error carries the codec's own error as its source, \
                 got {source}"
            );
        }
        other => panic!("the unreadable row is a decode error, got {other:?}"),
    }
    match &read[2] {
        Ok(typed) => assert_eq!(typed.event, Wanted("after".to_owned())),
        Err(failure) => panic!("the row after the unreadable one is still read, got {failure}"),
    }
}

#[tokio::test]
async fn load_2_is_read_through_every_handle_onto_a_store() {
    let (store, _) = mixed().await;
    let erased: &dyn EventStore = &store;
    let journal = Journal::from(store.clone());
    let whole_store = InMemoryStore::default();
    for (nth, (event_type, payload)) in [
        (WANTED, "first"),
        (SIBLING, "sibling"),
        (DEEPER, "deeper"),
        (WANTED, "second"),
        (SIBLING, "sibling"),
        (WANTED, "third"),
    ]
    .into_iter()
    .enumerate()
    {
        whole_store
            .events()
            .append(
                &format!("stream-{nth}"),
                vec![AppendingEvent {
                    sequence: 1,
                    event_type,
                    payload: Bytes::from([TAG, payload.as_bytes()].concat()),
                    occurred_at: instant(nth),
                }],
            )
            .await
            .expect("appending a fixture row to the whole store succeeds");
    }

    let through_backend: Vec<String> = store
        .read(LoadQuery::of::<Wanted>())
        .await
        .expect("a read through the backend's own type is served")
        .map_ok(|typed| typed.event.0)
        .try_collect()
        .await
        .expect("every row read through the backend's own type is readable");
    let through_erased: Vec<String> = erased
        .read(LoadQuery::of::<Wanted>())
        .await
        .expect("a read through `&dyn EventStore` is served")
        .map_ok(|typed| typed.event.0)
        .try_collect()
        .await
        .expect("every row read through `&dyn EventStore` is readable");
    let through_journal: Vec<String> = journal
        .read(LoadQuery::of::<Wanted>())
        .await
        .expect("a read through a journal is served")
        .map_ok(|typed| typed.event.0)
        .try_collect()
        .await
        .expect("every row read through a journal is readable");

    let through_whole_store: Vec<String> = whole_store
        .read(LoadQuery::of::<Wanted>())
        .await
        .expect("a read through a whole in-memory store is served")
        .map_ok(|typed| typed.event.0)
        .try_collect()
        .await
        .expect("every row read through a whole in-memory store is readable");

    for (handle, read) in [
        ("the backend's own type", through_backend),
        ("`&dyn EventStore`", through_erased),
        ("a journal", through_journal),
        ("a whole in-memory store", through_whole_store),
    ] {
        assert_eq!(
            read,
            vec!["first", "second", "third"],
            "a read through {handle} is the same read"
        );
    }
}
