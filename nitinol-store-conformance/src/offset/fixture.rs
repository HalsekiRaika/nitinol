use nitinol_persistence::store::{OffsetScope, OffsetStore};

/// The part name the suite's framework-owned keys are rendered under.
const CONFORMANCE_PART: &str = "conformance";

/// The key a framework-owned consumer renders: inside the
/// [reserved namespace](nitinol_persistence::reserved), where the framework
/// puts its own consumers so their keys cannot meet a user-named one's.
///
/// A store has to take one of these like any other key, so the suite renders
/// one the way the framework does rather than naming it as a user would — which
/// `OffsetScope::new` refuses outright.
pub(crate) fn framework_owned(consumer: &str, partition: usize) -> OffsetScope {
    OffsetScope::framework_owned(CONFORMANCE_PART, consumer, partition)
}

/// What the store holds for `scope`.
pub(crate) async fn held<S: OffsetStore>(
    clause: &str,
    backend: &S,
    scope: &OffsetScope,
) -> Option<u64> {
    backend.load(scope).await.unwrap_or_else(|failure| {
        panic!(
            "{clause} is broken: loading the position filed under {:?} reported {failure}, and a \
             consumer cannot start without being told where it had got to",
            scope.as_str()
        )
    })
}

/// A save a clause requires to be taken.
///
/// Every save the suite makes is a strict move forward under a key no other
/// writer in the clause uses, which is what a correct consumer makes. A save
/// that would not carry an offset forward is `OFS-4`'s subject and the
/// contract leaves it to the backend, so the suite never makes one.
pub(crate) async fn saved<S: OffsetStore>(
    clause: &str,
    backend: &S,
    scope: &OffsetScope,
    sequence: u64,
) {
    backend
        .save(scope, sequence, None)
        .await
        .unwrap_or_else(|failure| {
            panic!(
                "{clause} is broken: saving the position {sequence} under {:?}, which no other \
                 writer saves under and which the store has not been carried as far as, reported \
                 {failure}",
                scope.as_str()
            )
        });
}
