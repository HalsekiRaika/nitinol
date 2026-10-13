//! What an offset key is, and why two kinds of key cannot meet.
//!
//! ```gherkin
//!   Scenario: a user-named scope renders verbatim
//!     Given a name outside the reserved namespace
//!     When a user-named scope is constructed from it
//!     Then as_str answers with the name itself
//!
//!   Scenario: a user cannot name a key inside the reserved namespace
//!     Given a name inside the reserved namespace
//!     When a user-named scope is constructed from it
//!     Then the construction is refused
//!
//!   Scenario: every framework-owned key is inside the reserved namespace
//!     Given a part name, a name and a partition number
//!     When a framework-owned scope is constructed from them
//!     Then the rendered key is inside the reserved namespace
//!     And a user-named scope cannot be named that key
//!
//!   Scenario: the framework-owned rendering is injective
//!     Given two triples differing in at least one element
//!     And names containing "." and names ending in decimal digits
//!     When each is rendered
//!     Then the rendered keys differ
//!
//!   Scenario: one store, two kinds of consumer
//!     Given one offset store
//!     When a user-named scope and a framework-owned scope each save a position
//!     Then each reads back its own and neither reads the other's
//! ```
//!
//! This file is the whole executable record of the obligation `OFS-5` names:
//! that the keys *cannot* collide is `OffsetScope`'s, rather than a store's or
//! each consumer's. A store is told the rendered string and nothing else, so if
//! two kinds of consumer could render one string there would be no table a
//! deployment could share between them.
//!
//! The refusal half and the rendering half are two parts of one statement and
//! neither stands alone. A constructor that refused reserved names but rendered
//! framework-owned keys outside the namespace would keep the refusal clause and
//! let a user name a framework key; one that rendered inside the namespace but
//! accepted reserved user names would keep the rendering clause and let a user
//! name one too.
//!
//! Run with: `cargo test -p nitinol-persistence --test offset_scope`.

use std::collections::HashMap;

use nitinol_persistence::is_within_reserved_namespace;
use nitinol_persistence::store::{InMemoryOffsetStore, OffsetScope, OffsetStore};

/// Why an in-memory offset call cannot fail: it reads and writes a map behind a
/// lock no holder panics under.
const STORE_ANSWERS: &str = "an in-memory offset store answers every call";

/// Names a user may name a scope: outside the reserved namespace, including the
/// ones a naive prefix test would wrongly claim.
///
/// `""` is listed deliberately: the boundary the framework reserves classifies
/// it as outside, so the refusal must not spill over onto it. The only thing a
/// user-named scope refuses is a reserved name.
const NAMES_A_USER_MAY_NAME: &[&str] = &[
    "orders",
    "order-totals",
    "nitinolx",
    "nitinol-1",
    "nitinol_saga",
    "my.nitinol",
    "order-7",
    "",
];

/// Names inside the reserved namespace, which belongs to the framework's own
/// keys.
const NAMES_THE_FRAMEWORK_RESERVED: &[&str] =
    &["nitinol", "nitinol.saga", "nitinol.saga.dead_letter"];

/// Part name, name and partition number, as a framework-owned consumer hands
/// them over.
///
/// Chosen so that the pairs a weaker rendering folds together are present:
///
/// * a name that itself ends in what could be read as a partition number
///   (`"orders.1"` against `("orders", 1)`);
/// * a name whose trailing digits could be read as part of the number
///   (`("a", 75)` against `("a.7", 5)`, `("a1", 0)` against `("a", 10)`), which
///   a rendering that left out the separator before the number would fold;
/// * two names differing only in a separator (`"a.b"` against `"a_b"`), which a
///   rendering that kept itself injective by *restricting the name* — replacing
///   or stripping separators in it — would fold. The name is the one element
///   the rendering may not constrain: it comes from a deployment's own wiring,
///   and the part name is where the constraint belongs;
/// * a part name and a name that could be read as each other's continuation
///   (`("saga-deliveryx", "")` against `("saga-delivery", "x")`), which a
///   rendering missing the separator between the two would fold;
/// * an empty name, a name that is a separator, and a name inside the reserved
///   namespace — which a framework-owned consumer may carry, because the key it
///   renders is inside the namespace either way.
const FRAMEWORK_OWNED_TRIPLES: &[(&str, &str, usize)] = &[
    ("saga-delivery", "orders", 0),
    ("saga-delivery", "orders", 1),
    ("saga-delivery", "orders", 10),
    ("saga-delivery", "orders.1", 0),
    ("saga-delivery", "orders1", 0),
    ("saga-delivery", "a", 10),
    ("saga-delivery", "a", 75),
    ("saga-delivery", "a1", 0),
    ("saga-delivery", "a.7", 5),
    ("saga-delivery", "a.b", 0),
    ("saga-delivery", "a_b", 0),
    ("saga-delivery", "x", 0),
    ("saga-deliveryx", "", 0),
    ("saga-delivery", "", 0),
    ("saga-delivery", "", 1),
    ("saga-delivery", ".", 0),
    ("saga-delivery", "nitinol.orders", 0),
    ("saga-manager", "orders", 0),
    ("saga-deadline", "orders", 0),
];

fn framework_owned(triple: &(&'static str, &str, usize)) -> OffsetScope {
    OffsetScope::framework_owned(triple.0, triple.1, triple.2)
}

// A user names its own keys

/// A stored row is found under the name its consumer carries, so a scope must
/// neither prefix nor decorate it.
#[test]
fn a_user_named_scope_renders_the_name_verbatim() {
    for name in NAMES_A_USER_MAY_NAME {
        assert_eq!(
            OffsetScope::new(*name).as_str(),
            *name,
            "a user-named scope is the name itself: a rendering that added \
             anything would move every row a consumer had already written"
        );
    }
}

/// The refusal is what makes the separation a property of the type rather than
/// of a convention a caller follows.
#[test]
fn a_user_named_scope_refuses_a_name_inside_the_reserved_namespace() {
    for name in NAMES_THE_FRAMEWORK_RESERVED {
        let named_as = (*name).to_owned();
        let outcome = std::panic::catch_unwind(move || OffsetScope::new(named_as));

        let panicked = outcome.expect_err(&format!(
            "a user-named scope must refuse {name:?}: the reserved namespace is \
             where the framework's own keys are rendered, so a user able to name \
             one there could read a framework consumer's position as its own"
        ));
        let message = panicked
            .downcast_ref::<String>()
            .expect("the shared identifier refusal panics with a formatted String");
        assert!(
            message.contains("reserved namespace") && message.contains(name),
            "the refusal must be identifiable as a reserved-namespace violation \
             and name the value that violated it, got {message:?}"
        );
    }
}

// The framework names its own, under the root it reserved

/// Every framework-owned key is inside the reserved namespace, which is the
/// half of the disjointness the rendering owes.
#[test]
fn every_framework_owned_key_renders_inside_the_reserved_namespace() {
    for triple in FRAMEWORK_OWNED_TRIPLES {
        let scope = framework_owned(triple);

        assert!(
            is_within_reserved_namespace(scope.as_str()),
            "a framework-owned scope renders inside the framework's own \
             namespace so that no user-named scope can render the same key; \
             {triple:?} rendered {scope:?}"
        );
    }
}

/// And the other half: the key a framework-owned consumer renders is one a user
/// is refused outright, so the two key spaces are disjoint by construction.
#[test]
fn a_user_cannot_be_named_a_framework_owned_key() {
    for triple in FRAMEWORK_OWNED_TRIPLES {
        let key = framework_owned(triple).as_str().to_owned();
        let named_as = key.clone();

        let outcome = std::panic::catch_unwind(move || OffsetScope::new(named_as));

        assert!(
            outcome.is_err(),
            "a user named {key:?} must be refused rather than silently share a \
             framework-owned consumer's row"
        );
    }
}

/// The part name is the one element the rendering constrains, because it is
/// what the recovery of the three elements splits on. A part that was empty or
/// carried a separator would make the rendering ambiguous, and a part comes
/// from a literal in a framework crate rather than from data — so it is refused
/// the way every programming error in this crate is.
#[test]
fn a_part_name_that_would_make_the_rendering_ambiguous_is_refused() {
    for part in ["", "saga.delivery", ".", "a.b.c"] {
        let outcome = std::panic::catch_unwind(|| OffsetScope::framework_owned(part, "orders", 0));

        assert!(
            outcome.is_err(),
            "a part name of {part:?} must be refused: the rendering recovers the \
             part by splitting at a separator, so an empty one or one carrying a \
             separator would have two different triples render one key"
        );
    }
}

/// The same triple renders the same key, which is what has a restarted consumer
/// find the row it wrote.
#[test]
fn one_triple_renders_one_key() {
    for triple in FRAMEWORK_OWNED_TRIPLES {
        assert_eq!(
            framework_owned(triple),
            framework_owned(triple),
            "{triple:?} must render the same key every time, or a consumer would \
             lose its position across a restart"
        );
    }
}

/// And triples that differ render keys that differ — for every name, including
/// the ones a weaker rendering folds together.
#[test]
fn the_framework_owned_rendering_is_injective() {
    let mut rendered: HashMap<String, (&str, &str, usize)> = HashMap::new();

    for triple in FRAMEWORK_OWNED_TRIPLES {
        let key = framework_owned(triple).as_str().to_owned();

        if let Some(already) = rendered.insert(key.clone(), *triple) {
            panic!(
                "{triple:?} and {already:?} both render {key:?}: the rendering \
                 must be injective in the part name, the name and the partition \
                 number together, whatever the name contains"
            );
        }
    }
}

// What a store is told is the rendered string, so values are compared by it

/// Two values rendering one string are one key: a store keyed by the rendering
/// answers both with one position, so the type a consumer holds must compare
/// and hash the same way.
#[test]
fn two_values_rendering_one_string_are_one_key() {
    let mut positions: HashMap<OffsetScope, u64> = HashMap::new();
    positions.insert(OffsetScope::new("orders"), 11);
    positions.insert(framework_owned(&("saga-delivery", "orders", 0)), 22);

    assert_eq!(
        positions.get(&OffsetScope::new("orders")),
        Some(&11),
        "a user-named scope constructed again from the same name is the same \
         key, by equality and by hash both"
    );
    assert_eq!(
        positions.get(&framework_owned(&("saga-delivery", "orders", 0))),
        Some(&22),
        "a framework-owned scope constructed again from the same triple is the \
         same key"
    );
    assert_eq!(
        positions.get(&OffsetScope::new("order-totals")),
        None,
        "a scope rendering another string is another key"
    );
}

/// A consumer holds its scope for as long as it runs, so a clone of one is the
/// same key.
#[test]
fn a_clone_of_a_scope_is_the_same_key() {
    let scope = framework_owned(&("saga-delivery", "orders", 0));
    let held = scope.clone();

    assert_eq!(
        held.as_str(),
        scope.as_str(),
        "a clone renders what the original renders"
    );
    assert_eq!(held, scope, "and compares equal to it");
}

// One store, two kinds of consumer

/// The same text on both sides, so nothing but the rendering keeps the two
/// apart. This is what has one provider table serve every kind of consumer.
#[tokio::test]
async fn one_store_serves_both_kinds_without_either_reading_the_other() {
    let store = InMemoryOffsetStore::default();
    let user_named = OffsetScope::new("orders");
    let framework_owned_scope = framework_owned(&("saga-delivery", "orders", 0));

    store
        .save(&user_named, 11, None)
        .await
        .expect(STORE_ANSWERS);
    store
        .save(&framework_owned_scope, 22, None)
        .await
        .expect(STORE_ANSWERS);

    assert_eq!(
        store.load(&user_named).await.expect(STORE_ANSWERS),
        Some(11),
        "the user-named consumer reads back its own position"
    );
    assert_eq!(
        store
            .load(&framework_owned_scope)
            .await
            .expect(STORE_ANSWERS),
        Some(22),
        "and the framework-owned consumer reads back its own from the same \
         store, so the two share one stored progress cache without sharing a key"
    );
}

/// A scope nothing has saved for has no position while another holds one, which
/// is what lets a consumer tell `never started` from `started at zero`.
#[tokio::test]
async fn a_scope_with_no_row_yet_reads_as_absent() {
    let store = InMemoryOffsetStore::default();
    let held = OffsetScope::new("orders");
    store.save(&held, 11, None).await.expect(STORE_ANSWERS);

    assert_eq!(
        store
            .load(&framework_owned(&("saga-delivery", "orders", 3)))
            .await
            .expect(STORE_ANSWERS),
        None,
        "a consumer that has never saved has no position, even while another \
         key in the same store holds one"
    );
}
