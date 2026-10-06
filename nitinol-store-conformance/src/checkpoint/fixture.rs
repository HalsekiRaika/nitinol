use std::sync::Arc;

use nitinol_persistence::store::{CheckpointScope, CheckpointStore};
use nitinol_persistence::RESERVED_NAMESPACE;

/// A consumer of one kind, filing its position under the text it was given.
///
/// The rendered key is the whole of what a store is told, so a scope the
/// clauses can name any key with is the whole of what they need to construct.
pub(crate) struct AConsumer {
    rendered: String,
}

impl AConsumer {
    pub(crate) fn rendering(key: impl Into<String>) -> Self {
        Self {
            rendered: key.into(),
        }
    }
}

impl CheckpointScope for AConsumer {
    fn checkpoint_key(&self) -> &str {
        &self.rendered
    }
}

/// A consumer of a second kind, rendering the text it was given just as
/// [`AConsumer`] does.
///
/// A second type is what `CKP-5` needs to have anything to judge: the clause is
/// that the rendered key is the whole of what a store is told, and two kinds
/// rendering one key cannot be stated with one type. A projection resuming
/// after the framework grew a second kind of consumer is the live case — its
/// rows were written through one type and are read back under the same text.
pub(crate) struct AConsumerOfAnotherKind {
    rendered: String,
}

impl AConsumerOfAnotherKind {
    pub(crate) fn rendering(key: impl Into<String>) -> Self {
        Self {
            rendered: key.into(),
        }
    }
}

impl CheckpointScope for AConsumerOfAnotherKind {
    fn checkpoint_key(&self) -> &str {
        &self.rendered
    }
}

/// The key a framework-owned consumer renders: inside the
/// [reserved namespace](nitinol_persistence::reserved), where the framework
/// puts its own consumers so their keys cannot meet a projection's.
///
/// Assembled here rather than taken from an identifier type, because every
/// user-facing identifier constructor refuses such a name — which is what keeps
/// the two key spaces apart, and therefore why a store has to take one of these
/// like any other key.
pub(crate) fn framework_owned(consumer: &str) -> AConsumer {
    AConsumer::rendering(format!("{RESERVED_NAMESPACE}.conformance.{consumer}"))
}

/// What the store holds for `scope`.
pub(crate) async fn held<S: CheckpointStore>(
    clause: &str,
    backend: &Arc<S>,
    scope: &dyn CheckpointScope,
) -> Option<u64> {
    backend.load(scope).await.unwrap_or_else(|failure| {
        panic!(
            "{clause} is broken: loading the position filed under {:?} reported {failure}, and a \
             consumer cannot start without being told where it had got to",
            scope.checkpoint_key()
        )
    })
}

/// A save a clause requires to be taken.
///
/// Every save the suite makes is a strict move forward under a key no other
/// writer in the clause uses, which is what a correct consumer makes. A save
/// that would not carry a checkpoint forward is `CKP-4`'s subject and the
/// contract leaves it to the backend, so the suite never makes one.
pub(crate) async fn saved<S: CheckpointStore>(
    clause: &str,
    backend: &Arc<S>,
    scope: &dyn CheckpointScope,
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
                scope.checkpoint_key()
            )
        });
}
