use async_trait::async_trait;

/// Supplies the transaction handle used by ExactlyOnce projection delivery.
///
/// Wiring a `TxProvider` into a projector via
/// [`crate::ProjectorProps::with_tx_provider`] fixes the delivery mode to
/// `ExactlyOnce`. For each event the projector process calls [`begin`] to
/// get a transaction, runs [`crate::Projector::project`] against that
/// transaction, saves the checkpoint inside the same transaction, then calls
/// [`commit`]. If `project` or the checkpoint save fails, the transaction is
/// handed to [`rollback`] and the checkpoint is not advanced, so the same
/// event will be redelivered on the next attempt.
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
