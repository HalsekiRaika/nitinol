//! A load query is built up one condition at a time.
//!
//! ```gherkin
//!   Scenario: every condition named through a combinator
//!     Given the default query
//!     When each condition is added with its `with_*` combinator
//!     Then the query carries every one of them, whichever order they came in
//!
//!   Scenario: a constructor followed by combinators
//!     Given a query built by one of the single-condition constructors
//!     When further conditions are added with combinators
//!     Then the constructor's condition is still there beside them
//!
//!   Scenario: a stream key of any borrowed form
//!     Then `with_stream` takes whatever `by_stream` takes
//! ```
//!
//! The failure these exclude is a combinator written the way the constructors
//! are, `Self { field, ..Default::default() }`: it compiles, it sets the one
//! condition it names, and it silently drops every condition set before it — so
//! a query meant as a conjunction reaches the store as a single condition and
//! answers with more than was asked for.

use nitinol_persistence::{AggregateId, EventType, Family, LoadQuery, MaterializedPath, TypeName};

const NAMED: EventType = EventType::new(Family::new("combinator"), TypeName::new("Named"));

/// Every condition a query carries, in a form that compares.
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

fn conditions(query: LoadQuery) -> Conditions {
    Conditions {
        stream_key: query.stream_key,
        event_type_prefix: query.event_type_prefix,
        from_global_sequence: query.from_global_sequence,
        from_stream_sequence: query.from_stream_sequence,
        limit: query.limit,
    }
}

fn every_condition() -> Conditions {
    Conditions {
        stream_key: Some("stream-a".to_owned()),
        event_type_prefix: Some(NAMED.to_path()),
        from_global_sequence: Some(7),
        from_stream_sequence: Some(3),
        limit: Some(5),
    }
}

#[test]
fn every_combinator_adds_its_condition_without_dropping_the_others() {
    let forwards = LoadQuery::default()
        .with_stream("stream-a")
        .with_event_type_prefix(NAMED.to_path())
        .with_from_global(7)
        .with_from_stream_sequence(3)
        .with_limit(5);
    let backwards = LoadQuery::default()
        .with_limit(5)
        .with_from_stream_sequence(3)
        .with_from_global(7)
        .with_event_type_prefix(NAMED.to_path())
        .with_stream("stream-a");

    assert_eq!(
        conditions(forwards),
        every_condition(),
        "each combinator sets the condition it names and keeps every condition \
         set before it"
    );
    assert_eq!(
        conditions(backwards),
        every_condition(),
        "the order the combinators are applied in does not change the query: \
         none of them resets a condition another one set"
    );
}

#[test]
fn a_constructors_condition_survives_the_combinators_added_after_it() {
    let query = LoadQuery::by_event_type(NAMED)
        .with_stream("stream-a")
        .with_from_global(7)
        .with_from_stream_sequence(3)
        .with_limit(5);

    assert_eq!(
        conditions(query),
        every_condition(),
        "the existing constructors stay, and a combinator extends what one of \
         them built rather than replacing it"
    );
}

#[test]
fn with_stream_takes_every_borrowed_form_of_a_stream_key() {
    let aggregate = AggregateId::new("stream-a");
    let owned = String::from("stream-a");

    for (form, query) in [
        ("&str", LoadQuery::default().with_stream("stream-a")),
        ("&String", LoadQuery::default().with_stream(&owned)),
        ("&AggregateId", LoadQuery::default().with_stream(&aggregate)),
    ] {
        assert_eq!(
            query.stream_key.as_deref(),
            Some("stream-a"),
            "a stream key given as {form} names the stream it borrows as a str"
        );
    }
}
