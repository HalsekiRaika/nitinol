//! What a typed read serves over this provider's journal.
//!
//! ```gherkin
//!   Scenario: the rows of one type
//!     Given rows of a declaration, of its sibling, and of a declaration whose
//!          family is the first one's whole path
//!     When the first declaration is read as its type
//!     Then what comes back is what a load of its path serves, with the
//!          deeper declaration's rows left out, at the positions the load
//!          serves them at
//!
//!   Scenario: the rest of the query
//!     Then the stream and the limit apply to the read type's rows, as the
//!          store selects them
//! ```
//!
//! The read narrows the query to the type's path before the store sees it, so
//! the type is selected by the prefix comparison the journal's index serves.
//! The second scenario is the one a read that loaded everything and sorted the
//! rows out afterwards cannot pass: the limit would count rows of other types.
//!
//! Running any of this needs a database. Where none can be reached the clause
//! judges nothing, which is not evidence that it holds.

use super::fixture;
use super::harness;

use std::fmt;

use bytes::Bytes;
use fixture::{committed, observe, typed};
use futures_util::TryStreamExt;
use linkme::distributed_slice;
use nitinol_eventsource::codec::Codec;
use nitinol_eventsource::Event;
use nitinol_persistence::store::{Journal, TypedLoad};
use nitinol_persistence::{
    EventRegistryEntry, EventType, Family, LoadQuery, TypeName, EVENT_REGISTRY,
};

const READ: EventType = EventType::new(Family::new("sqlx.typed"), TypeName::new("Read"));
const SIBLING: EventType = EventType::new(Family::new("sqlx.typed"), TypeName::new("Sibling"));
/// A declaration whose family is `READ`'s whole path, so a prefix naming `READ`
/// reaches its rows too.
const DEEPER: EventType = EventType::new(Family::new("sqlx.typed.Read"), TypeName::new("Deeper"));

/// The three neighbouring declarations, collected so that a store restores
/// every row a clause appends.
#[distributed_slice(EVENT_REGISTRY)]
static TYPED_READ_DECLARATIONS: EventRegistryEntry = EventRegistryEntry::new(
    concat!(module_path!(), "::typed_read_declarations"),
    &[READ, SIBLING, DEEPER],
);

#[derive(Debug, Clone, PartialEq)]
struct Read(String);

impl Event for Read {
    const EVENT_TYPE: EventType = READ;

    type Codec = Tagged;
}

/// A format that reads a payload carrying its tag and refuses anything else.
struct Tagged;

const TAG: &[u8] = b"read:";

#[derive(Debug)]
struct Untagged;

impl fmt::Display for Untagged {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the payload does not carry the `read:` tag")
    }
}

impl std::error::Error for Untagged {}

impl Codec<Read> for Tagged {
    type Error = Untagged;

    fn encode(value: &Read) -> Result<Bytes, Self::Error> {
        Ok(Bytes::from([TAG, value.0.as_bytes()].concat()))
    }

    fn decode(payload: &[u8]) -> Result<Read, Self::Error> {
        let text = payload.strip_prefix(TAG).ok_or(Untagged)?;
        let text = std::str::from_utf8(text).map_err(|_| Untagged)?;
        Ok(Read(text.to_owned()))
    }
}

/// The coordinates and value of every item a typed read serves, all of which
/// the caller requires to be readable.
async fn read(store: &Journal, query: LoadQuery) -> Vec<(String, u64, u64, String)> {
    store
        .read::<Read>(query)
        .await
        .unwrap_or_else(|failure| panic!("the store failed a typed read outright: {failure}"))
        .map_ok(|typed| {
            (
                typed.stream_key,
                typed.sequence,
                typed.global_sequence,
                typed.event.0,
            )
        })
        .try_collect()
        .await
        .unwrap_or_else(|failure| panic!("a typed read failed part-way through: {failure}"))
}

#[tokio::test]
async fn a_typed_read_is_the_load_of_its_path_with_its_own_rows_alone() {
    const STREAM: &str = "sqlx.typed.rows";
    const OTHER: &str = "sqlx.typed.rows.other";

    let Some(backend) = harness::postgres_backend().await else {
        return;
    };
    let store = backend.bring_up_store().await;
    committed(
        &store,
        STREAM,
        vec![
            typed(1, READ, b"read:first"),
            typed(2, SIBLING, b"read:sibling"),
            typed(3, DEEPER, b"read:deeper"),
        ],
    )
    .await;
    committed(&store, OTHER, vec![typed(1, READ, b"read:second")]).await;

    let loaded = observe(&store, LoadQuery::by_event_type(READ)).await;
    assert!(
        loaded.iter().any(|row| row.event_type == DEEPER),
        "a load of a declaration's path serves the rows of a declaration whose \
         family is that path, which is what leaves the exact comparison to the \
         reader"
    );
    let expected: Vec<(String, u64, u64, String)> = loaded
        .into_iter()
        .filter(|row| row.event_type == READ)
        .map(|row| {
            let value = <Tagged as Codec<Read>>::decode(&row.payload)
                .expect("every `READ` row of the fixture carries the tag");
            (row.stream_key, row.sequence, row.global_sequence, value.0)
        })
        .collect();

    let served = read(&store, LoadQuery::default()).await;

    assert_eq!(
        served, expected,
        "a typed read serves the rows of its own declaration that a load of its \
         path serves, in the same order and at the same positions, and neither \
         the sibling's rows nor the deeper declaration's"
    );
    assert_eq!(
        served
            .iter()
            .map(|(_, _, _, value)| value.as_str())
            .collect::<Vec<_>>(),
        vec!["first", "second"],
        "both streams' rows of the read declaration are served"
    );
}

#[tokio::test]
async fn a_typed_read_is_narrowed_before_the_store_applies_the_rest_of_the_query() {
    const STREAM: &str = "sqlx.typed.conjunction";
    const OTHER: &str = "sqlx.typed.conjunction.other";

    let Some(backend) = harness::postgres_backend().await else {
        return;
    };
    let store = backend.bring_up_store().await;
    committed(&store, OTHER, vec![typed(1, READ, b"read:elsewhere")]).await;
    committed(
        &store,
        STREAM,
        vec![
            typed(1, READ, b"read:first"),
            typed(2, SIBLING, b"read:sibling"),
            typed(3, SIBLING, b"read:sibling"),
            typed(4, READ, b"read:second"),
            typed(5, READ, b"read:third"),
        ],
    )
    .await;

    let served = read(
        &store,
        LoadQuery::default().with_stream(STREAM).with_limit(2),
    )
    .await;

    assert_eq!(
        served
            .iter()
            .map(|(_, _, _, value)| value.as_str())
            .collect::<Vec<_>>(),
        vec!["first", "second"],
        "the stream applies, and the limit counts the rows the store selected \
         for the read declaration's path: the other stream's row is not served, \
         and the sibling rows between do not take a place in the limit"
    );
}
