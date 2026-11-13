use async_trait::async_trait;

/// Supplies the transaction handle used by ExactlyOnce projection delivery.
///
/// - Wired in with [`crate::ProjectorProps::with_tx_provider`], which fixes the
///   delivery mode to `ExactlyOnce`. Each batch, at most
///   [`crate::ProjectorProps::with_batch_limit`] records, is one transaction:
///   `begin`, `project` for every record, the offset save, then `commit`. A
///   batch whose every record is skipped opens no transaction.
/// - If `begin`, a `project` or the offset save fails, the batch is left
///   unapplied: an open transaction goes to `rollback`, the offset stays, and
///   the batch is redelivered from its first record. Records after the failing
///   one are not projected.
/// - A failed `commit` is treated as uncommitted, whether the backend rejected
///   it or its outcome was lost, and the batch is redelivered; if it did land,
///   the batch is applied a second time. An implementation that can settle an
///   unknown outcome inside `commit` should do so rather than returning `Err`.
#[async_trait]
pub trait TxProvider: Send + Sync + 'static {
    /// The transaction a batch is applied in.
    type Tx: Send + 'static;
    /// The error [`begin`](Self::begin) and [`commit`](Self::commit) return.
    type Error: std::error::Error + Send + Sync + 'static;
    /// Opens a transaction for one batch.
    async fn begin(&self) -> Result<Self::Tx, Self::Error>;
    /// Commits `tx` once every record of the batch and its offset are in it.
    async fn commit(&self, tx: Self::Tx) -> Result<(), Self::Error>;
    /// Rolls back `tx` after a step of the batch failed.
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
