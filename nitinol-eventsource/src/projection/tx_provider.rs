use async_trait::async_trait;

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
