use crate::fixture::{LedgerNotOpen, LedgerRejection};

/// What an interpreter made of one command.
///
/// An interpreter classifies its own raw result into this once, at its own
/// boundary, and every clause of the suite then judges the same verdict.  A
/// suite that re-read a raw error its own way in each clause would be grading
/// its own reading of the interpreter rather than the interpreter.
#[derive(Debug)]
pub enum Interpreted<O> {
    /// The decision was carried out and its answer delivered.
    Answered(O),
    /// A domain rule refused the command.
    Refused(LedgerRejection),
    /// A creation collided with a ledger that already exists, so no decision
    /// was reached and there is no answer to deliver.
    AlreadyCreated,
    /// The machinery around the decision got in the way.
    Failed(Fault),
}

/// Why a question came back without an answer.
#[derive(Debug)]
pub enum Unanswered {
    /// The domain has no answer to give.
    Domain(LedgerNotOpen),
    /// The question never reached state that could answer it.
    Failed(Fault),
}

/// A failure of an interpreter's own machinery.
///
/// Erased on purpose: which store, mailbox or codec failed is the
/// interpreter's vocabulary, and the suite cannot name it for an interpreter
/// that does not exist yet.  Telling this apart from a refusal is the whole
/// point — what it *is* belongs to whoever built the machinery, and travels
/// with the fault as its own rendering.
#[derive(Debug, thiserror::Error)]
#[error("the machinery around a decision failed: {0}")]
pub struct Fault(Box<dyn std::error::Error + Send + Sync>);

impl Fault {
    pub fn new(source: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self(Box::new(source))
    }
}
