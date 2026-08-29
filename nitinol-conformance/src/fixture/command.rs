/// Open the ledger for a holder.
///
/// The only creation the fixture has: from an unopened ledger it is always
/// accepted, so a collision on the stream's genesis is the machinery's to
/// report and never a refusal the domain reached.
pub struct Open {
    pub holder: String,
}

/// Credit the ledger and charge it, in that order, as one settlement.
///
/// One command producing two facts whose order changes the outcome, and — when
/// there is nothing to credit and nothing to charge — an acceptance that
/// produces none at all.  A charge the credit cannot fund is refused.
///
/// It lives beside [`Open`] because the two are the ledger's whole command
/// vocabulary: neither means anything without the other, and a fixture that
/// gained a third command would gain it here.
pub struct Settle {
    pub credit: u64,
    pub charge: u64,
}
