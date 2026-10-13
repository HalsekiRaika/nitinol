use nitinol_persistence::store::OffsetScope;

use super::fixture;
use super::OffsetStoreUnderTest;

/// Put an offset store through every clause a particular interleaving is not
/// needed for.
///
/// Each clause constructs the situation it judges, so a pass is evidence about
/// the store rather than about how a run happened to be scheduled. Nothing here
/// spawns anything, which is why no executor is asked for: a clause that needed
/// one would belong in
/// [`verify_under_concurrency`](super::verify_under_concurrency).
///
/// The clauses run in the order below, and the first one broken panics naming
/// its `OFS` label.
pub async fn verify_deterministic<S: OffsetStoreUnderTest>(store: &S) {
    a_scope_nothing_has_saved_for_has_no_position(store).await;
    a_saved_position_is_what_a_load_answers_with(store).await;
    a_move_forward_is_taken_however_far_it_reaches(store).await;
    the_rendered_key_is_the_whole_of_what_a_store_is_told(store).await;
}

/// OFS-1. A scope nothing has saved a position for is answered with
/// `Ok(None)` — no position at all, rather than a position of zero.
async fn a_scope_nothing_has_saved_for_has_no_position<S: OffsetStoreUnderTest>(store: &S) {
    const CLAUSE: &str = "OFS-1";

    let backend = store.bring_up().await;
    let never_saved = OffsetScope::new("ofs.conformance.unsaved");

    let answer = fixture::held(CLAUSE, &backend, &never_saved).await;

    assert_eq!(
        answer,
        None,
        "{CLAUSE} is broken: nothing has saved a position under {:?}, and its load answered with \
         {answer:?}; a consumer that has never run has to be told apart from one that stopped at \
         the store's first record, and a position of zero is what the second looks like",
        never_saved.as_str()
    );
}

/// OFS-2. The position a `save` was handed is the position a load answers with.
async fn a_saved_position_is_what_a_load_answers_with<S: OffsetStoreUnderTest>(store: &S) {
    const CLAUSE: &str = "OFS-2";

    let backend = store.bring_up().await;
    let consumer = OffsetScope::new("ofs.conformance.roundtrip");
    fixture::saved(CLAUSE, &backend, &consumer, 17).await;

    let answer = fixture::held(CLAUSE, &backend, &consumer).await;

    assert_eq!(
        answer,
        Some(17),
        "{CLAUSE} is broken: the position 17 was saved under {:?} and its load answered with \
         {answer:?}",
        consumer.as_str()
    );
}

/// OFS-3. A position beyond the one the store holds is taken however far beyond
/// it reaches: a store asks for no particular next number.
///
/// A consumer following the store's whole order moves over records it has no
/// handler for, and one partition of a delivery host counts what another
/// partition finished as finished, so the position such a consumer saves is
/// not the one before it plus one. A store that required the next number would
/// refuse every save either of them makes.
async fn a_move_forward_is_taken_however_far_it_reaches<S: OffsetStoreUnderTest>(store: &S) {
    const CLAUSE: &str = "OFS-3";

    let backend = store.bring_up().await;
    let consumer = OffsetScope::new("ofs.conformance.skipping");

    fixture::saved(CLAUSE, &backend, &consumer, 5).await;
    for position in [9, 40] {
        fixture::saved(CLAUSE, &backend, &consumer, position).await;
        let answer = fixture::held(CLAUSE, &backend, &consumer).await;
        assert_eq!(
            answer,
            Some(position),
            "{CLAUSE} is broken: {:?} was carried to {position} from a position below it, \
             skipping the numbers in between, and its load answered with {answer:?}",
            consumer.as_str()
        );
    }
}

/// OFS-5. The rendered key is the whole of what a store is told: positions
/// under keys that differ are kept apart, one key is one position whatever type
/// rendered it, and a key inside the reserved namespace is stored like any
/// other.
async fn the_rendered_key_is_the_whole_of_what_a_store_is_told<S: OffsetStoreUnderTest>(store: &S) {
    const CLAUSE: &str = "OFS-5";
    const SHARED: &str = "ofs.conformance.keys.one-key-two-kinds";

    let backend = store.bring_up().await;

    // Keys that differ are kept apart, in both directions: a key nothing has
    // saved for still has no position while another holds one, and once both
    // hold one neither answers with the other's.
    let mine = OffsetScope::new("ofs.conformance.keys.mine");
    let anothers = OffsetScope::new("ofs.conformance.keys.anothers");
    fixture::saved(CLAUSE, &backend, &anothers, 11).await;
    let answer = fixture::held(CLAUSE, &backend, &mine).await;
    assert_eq!(
        answer,
        None,
        "{CLAUSE} is broken: the only position the store holds was saved under {:?}, and a load \
         of {:?} answered with {answer:?}; each consumer would resume from wherever another one \
         had got to",
        anothers.as_str(),
        mine.as_str()
    );
    fixture::saved(CLAUSE, &backend, &mine, 4).await;
    for (consumer, position) in [(&mine, 4), (&anothers, 11)] {
        let answer = fixture::held(CLAUSE, &backend, consumer).await;
        assert_eq!(
            answer,
            Some(position),
            "{CLAUSE} is broken: {:?} was carried to {position} and its load answered with \
             {answer:?}; a save under one key reaches no other",
            consumer.as_str()
        );
    }

    // Two values rendering one string are one key, which is what has a
    // consumer's rows found again after a restart built its scope afresh.
    let built_once = OffsetScope::new(SHARED);
    let built_again = OffsetScope::new(SHARED);
    fixture::saved(CLAUSE, &backend, &built_once, 23).await;
    let answer = fixture::held(CLAUSE, &backend, &built_again).await;
    assert_eq!(
        answer,
        Some(23),
        "{CLAUSE} is broken: a position saved under {SHARED:?} through one scope value was \
         answered as {answer:?} through a second value rendering the same key; what a store files \
         a position under is the text, and nothing about the value that carried it"
    );

    // And the same for a framework-owned key, where the three elements rather
    // than a name are what two values agree on.
    let triple_once = fixture::framework_owned("keys.one-key-two-values", 0);
    let triple_again = fixture::framework_owned("keys.one-key-two-values", 0);
    fixture::saved(CLAUSE, &backend, &triple_once, 31).await;
    let answer = fixture::held(CLAUSE, &backend, &triple_again).await;
    assert_eq!(
        answer,
        Some(31),
        "{CLAUSE} is broken: a position saved under {:?} was answered as {answer:?} through a \
         second value rendering the same key",
        triple_once.as_str()
    );

    // Framework-owned keys that differ are kept apart like any other.
    let another_partition = fixture::framework_owned("keys.one-key-two-values", 1);
    let answer = fixture::held(CLAUSE, &backend, &another_partition).await;
    assert_eq!(
        answer,
        None,
        "{CLAUSE} is broken: {:?} holds a position while only {:?} was saved for; keys that \
         render different strings are different keys, framework-owned or not",
        another_partition.as_str(),
        triple_once.as_str()
    );

    // A framework-owned consumer renders inside the reserved namespace on
    // purpose, so a store reading that namespace as something to validate would
    // keep the framework's own consumers from recording anything.
    let framework = fixture::framework_owned("keys.framework-owned", 0);
    fixture::saved(CLAUSE, &backend, &framework, 6).await;
    let answer = fixture::held(CLAUSE, &backend, &framework).await;
    assert_eq!(
        answer,
        Some(6),
        "{CLAUSE} is broken: the position 6 was saved under {:?}, a key inside the framework's \
         reserved namespace, and its load answered with {answer:?}",
        framework.as_str()
    );
}
