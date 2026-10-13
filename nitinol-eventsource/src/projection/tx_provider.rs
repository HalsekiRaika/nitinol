use async_trait::async_trait;

/// Supplies the transaction handle used by ExactlyOnce projection delivery.
///
/// Wiring a `TxProvider` into a projector via
/// [`crate::ProjectorProps::with_tx_provider`] fixes the delivery mode to
/// `ExactlyOnce`. For each event the projector process calls [`begin`] to
/// get a transaction, runs [`crate::Projector::project`] against that
/// transaction, saves the offset inside the same transaction, then calls
/// [`commit`]. If [`begin`], `project` or the offset save fails, the event
/// is left unapplied: any transaction that was started is handed to
/// [`rollback`], the offset is not advanced, and the subscription cursor
/// stays put so the same event is redelivered on the next poll.
///
/// # A failed `commit` is treated as uncommitted
///
/// [`commit`] answers with a single `Err`, and it has already consumed the
/// transaction.  Two different situations therefore arrive here as the same
/// value: a commit the backend rejected and rolled back, and a commit whose
/// outcome never came back — a connection lost after the statement was sent,
/// where the transaction may well have landed.  The framework cannot tell them
/// apart, and there is nothing left to hand to [`rollback`].
///
/// Both are treated as **not committed**: the offset stays where it was and
/// the event is redelivered.  The alternative — assuming the commit landed —
/// loses the event permanently whenever it did not.  The cost is that an
/// `ExactlyOnce` projector degrades to at-least-once across an unknown commit:
/// if the transaction did commit, the redelivered event is applied a second
/// time.  Implementations that can tell a rejected commit from an unknown one,
/// and that can settle the unknown case themselves (by retrying or querying the
/// backend inside [`commit`]), should do so rather than returning `Err`.
///
/// [`begin`]: TxProvider::begin
/// [`commit`]: TxProvider::commit
/// [`rollback`]: TxProvider::rollback
#[async_trait]
pub trait TxProvider: Send + Sync + 'static {
    type Tx: Send + 'static;
    type Error: std::error::Error + Send + Sync + 'static;
    async fn begin(&self) -> Result<Self::Tx, Self::Error>;
    async fn commit(&self, tx: Self::Tx) -> Result<(), Self::Error>;
    async fn rollback(&self, tx: Self::Tx);
}

#[async_trait]
pub(crate) trait ErasedTxProvider<Tx: Send>: Send + Sync {
    async fn begin(&self) -> Result<Tx, Box<dyn std::error::Error + Send + Sync>>;
    async fn commit(&self, tx: Tx) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;
    async fn rollback(&self, tx: Tx);
}

#[async_trait]
impl<TP: TxProvider> ErasedTxProvider<TP::Tx> for TP {
    async fn begin(&self) -> Result<TP::Tx, Box<dyn std::error::Error + Send + Sync>> {
        TxProvider::begin(self)
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
    }

    async fn commit(&self, tx: TP::Tx) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        TxProvider::commit(self, tx)
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
    }

    async fn rollback(&self, tx: TP::Tx) {
        TxProvider::rollback(self, tx).await;
    }
}
