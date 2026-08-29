/// A domain rule that refused a command.
///
/// Every variant is a statement about the command against the ledger's current
/// state.  Nothing here describes a store, a mailbox or a codec: an interpreter
/// that carried a failure of its own machinery back through this type would
/// make it indistinguishable from a verdict the ledger reached.
#[derive(Debug, PartialEq, thiserror::Error)]
pub enum LedgerRejection {
    #[error("the ledger has already been opened")]
    AlreadyOpen,
    #[error("the ledger has not been opened")]
    NotOpen,
    #[error("charging {requested} leaves the ledger short: only {available} is funded")]
    Underfunded { requested: u64, available: u64 },
}

/// The ledger has no answer yet, because it has not been opened.
///
/// The domain's own answer to a question it cannot answer, kept apart from the
/// machinery that carried the question.
#[derive(Debug, thiserror::Error)]
#[error("the ledger has not been opened, so it holds nothing to report")]
pub struct LedgerNotOpen;
