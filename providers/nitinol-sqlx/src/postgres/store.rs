use nitinol_persistence::provider::{ProjectionOffsetSlot, SagaOffsetSlot, Store};
use nitinol_persistence::store::Journal;
use nitinol_persistence::EventRegistryError;
use sqlx::PgPool;

use crate::postgres::{PostgresEventStore, PostgresOffsetStore};

/// This backend as one [`Store`]: the journal and both offset slots, and
/// no snapshot store.
///
/// There is no Postgres [`SnapshotStore`] in this crate, so the snapshot slot
/// is left unimplemented rather than filled from somewhere else — wiring that
/// needs snapshots is refused while it is compiled. A deployment that wants
/// them names a handle for that slot itself in a
/// [`CustomStore`](nitinol_persistence::provider::CustomStore), which is what
/// mixing backends has to look like.
///
/// # One table, two slots
///
/// Both offset slots answer with **one** store over one table, which is
/// this backend's deployment: what keeps a projection's position apart from
/// the framework's own saga attachments' progress there is the key each
/// consumer renders, not a table each. The slots are still independent —
/// neither answers for what the other filed — which is the contract the
/// conformance suite runs.
///
/// # One pool, one region
///
/// The journal and the offsets share the pool because they share the
/// database, and one migration step creates both tables. Sharing a pool is an
/// implementation of identity rather than the contract: what makes two handles
/// one world is that they reach the same rows, which is `PRV-2`. Cloning this
/// store copies the pool handle, so a clone reaches those same rows.
///
/// [`SnapshotStore`]: nitinol_persistence::store::SnapshotStore
#[derive(Clone)]
pub struct PostgresStore {
    events: Journal,
    offsets: PostgresOffsetStore,
}

impl PostgresStore {
    /// Assemble this backend's store over `pool`.
    ///
    /// Fails where two event declarations in this binary claim one canonical
    /// path, which is what [`PostgresEventStore::new`] reports — the one
    /// failure that belongs at assembly rather than at the row it would have
    /// restored wrongly.
    pub fn new(pool: PgPool) -> Result<Self, EventRegistryError> {
        Ok(Self {
            events: Journal::from(PostgresEventStore::new(pool.clone())?),
            offsets: PostgresOffsetStore::new(pool),
        })
    }
}

impl Store for PostgresStore {
    fn events(&self) -> Journal {
        self.events.clone()
    }
}

impl ProjectionOffsetSlot for PostgresStore {
    type ProjectionOffsets = PostgresOffsetStore;

    fn projection_offsets(&self) -> PostgresOffsetStore {
        self.offsets.clone()
    }
}

impl SagaOffsetSlot for PostgresStore {
    type SagaOffsets = PostgresOffsetStore;

    fn saga_offsets(&self) -> PostgresOffsetStore {
        self.offsets.clone()
    }
}
