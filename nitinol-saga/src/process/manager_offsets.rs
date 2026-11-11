//! The saga-offset slot a manager records its sources' progress in, with the
//! backend's type out of the signature.

use async_trait::async_trait;
use nitinol_persistence::error::OffsetError;
use nitinol_persistence::store::{OffsetScope, OffsetStore};

/// The system's saga-offset slot, as the manager and its source partitions
/// hold it.
///
/// Erased rather than carried as a type parameter because the manager's type
/// is named, as it is, by the parts that address it — the deadline index asks
/// it for instances — and a backend type on it would reach every one of them.
/// What is inside is the slot's own clone, so every partition records in the
/// one table the delivery host records in.
///
/// Only what the manager uses is kept: a load, and a save that joins no
/// transaction. A source partition's offset settles nothing alongside it, so
/// the backend's `Tx` has no caller here.
pub(crate) struct SagaManagerOffsets(Box<dyn ErasedOffsets>);

impl SagaManagerOffsets {
    pub(crate) fn new<Cs>(offsets: Cs) -> Self
    where
        Cs: OffsetStore + Clone + 'static,
    {
        Self(Box::new(offsets))
    }

    /// [`OffsetStore::load`], unchanged.
    pub(crate) async fn load(&self, scope: &OffsetScope) -> Result<Option<u64>, OffsetError> {
        self.0.load(scope).await
    }

    /// [`OffsetStore::save`] outside any transaction.
    pub(crate) async fn save(&self, scope: &OffsetScope, sequence: u64) -> Result<(), OffsetError> {
        self.0.save(scope, sequence).await
    }
}

impl Clone for SagaManagerOffsets {
    fn clone(&self) -> Self {
        Self(self.0.clone_box())
    }
}

#[async_trait]
trait ErasedOffsets: Send + Sync {
    async fn load(&self, scope: &OffsetScope) -> Result<Option<u64>, OffsetError>;

    async fn save(&self, scope: &OffsetScope, sequence: u64) -> Result<(), OffsetError>;

    fn clone_box(&self) -> Box<dyn ErasedOffsets>;
}

#[async_trait]
impl<Cs> ErasedOffsets for Cs
where
    Cs: OffsetStore + Clone + 'static,
{
    async fn load(&self, scope: &OffsetScope) -> Result<Option<u64>, OffsetError> {
        OffsetStore::load(self, scope).await
    }

    async fn save(&self, scope: &OffsetScope, sequence: u64) -> Result<(), OffsetError> {
        OffsetStore::save(self, scope, sequence, None).await
    }

    fn clone_box(&self) -> Box<dyn ErasedOffsets> {
        Box::new(self.clone())
    }
}
