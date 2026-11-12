/// A domain rule that refused a command.
///
/// Every variant is a statement about the command against the ledger's current
/// state.  Nothing here describes a store, a mailbox or a codec: an interpreter
/// that carried a failure of its own machinery back through this type would
/// make it indistinguishable from a verdict the ledger reached.
#[derive(Debug, PartialEq, thiserror::Error)]
pub enum LedgerRejection {
    /// An [`Open`](crate::Open) was put to a ledger that is already open.
    #[error("the ledger has already been opened")]
    AlreadyOpen,
    /// A [`Settle`](crate::Settle) was put to a ledger that has not been opened.
    #[error("the ledger has not been opened")]
    NotOpen,
    /// A [`Settle`](crate::Settle) charged more than the balance and its credit fund.
    #[error("charging {requested} leaves the ledger short: only {available} is funded")]
    Underfunded {
        /// The amount the settlement charged.
        requested: u64,
        /// The balance plus the settlement's credit.
        available: u64,
    },
}

/// The ledger has no answer yet, because it has not been opened.
///
/// The domain's own answer to a question it cannot answer, kept apart from the
/// machinery that carried the question.
#[derive(Debug, thiserror::Error)]
#[error("the ledger has not been opened, so it holds nothing to report")]
pub struct LedgerNotOpen;
