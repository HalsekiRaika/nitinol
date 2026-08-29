/// How much the ledger holds.
pub struct Balance;

/// Who the ledger was opened for.
///
/// It lives beside [`Balance`] because the two are the ledger's whole question
/// vocabulary; they differ only in which part of the same state they report,
/// and both are unanswerable until the ledger has been opened.
pub struct Holder;
