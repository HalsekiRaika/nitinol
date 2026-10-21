use nitinol_persistence::provider::{ProjectionOffsetSlot, SagaOffsetSlot, SnapshotSlot, Store};
use nitinol_persistence::store::{Album, Journal};
use nitinol_persistence::EventRegistryError;
use sqlx::PgPool;

use crate::postgres::{PostgresEventStore, PostgresOffsetStore, PostgresSnapshotStore};

/// This backend as one [`Store`]: every slot — the journal, the snapshot store
/// and both offset slots — over one database.
///
/// A deployment naming this store needs no second backend beside it: wiring
/// that needs snapshots compiles, which is what an aggregate declaring
/// `Latest` recovery asks of the system it is spawned on. Naming a handle per
/// slot in a [`CustomStore`](nitinol_persistence::provider::CustomStore) is
/// still open to a deployment that wants another backend for one of them, and
/// is a choice rather than the only way to have snapshots over Postgres.
///
/// # One table per slot, and one shared by two
///
/// Both offset slots answer with **one** store over one table, which is
/// this backend's deployment: what keeps a projection's position apart from
/// the framework's own saga attachments' progress there is the key each
/// consumer renders, not a table each. The slots are still independent —
/// neither answers for what the other filed — which is the contract the
/// conformance suite runs. The journal and the snapshots have a table each,
/// because a snapshot is one row an aggregate replaces and an event is a row
/// appended under a sequence nothing may reuse.
///
/// # What a snapshot save does to the one before it
///
/// [`PostgresSnapshotStore`] keeps whichever save came last, a position behind
/// the stored one included; its own rustdoc is where that is written down.
///
/// # One pool, one region
///
/// Every slot shares the pool because they share the database, and one
/// migration step creates every table. Sharing a pool is an implementation of
/// identity rather than the contract: what makes two handles one world is that
/// they reach the same rows, which is `PRV-2`. Cloning this store copies the
/// pool handle, so a clone reaches those same rows.
#[derive(Clone)]
pub struct PostgresStore {
    events: Journal,
    snapshots: Album,
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
            snapshots: Album::from(PostgresSnapshotStore::new(pool.clone())),
            offsets: PostgresOffsetStore::new(pool),
        })
    }
}

impl Store for PostgresStore {
    fn events(&self) -> Journal {
        self.events.clone()
    }
}

impl SnapshotSlot for PostgresStore {
    fn snapshots(&self) -> Album {
        self.snapshots.clone()
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
