# nitinol-sqlx

A Postgres `EventStore` and `CheckpointStore` for `nitinol`, built on `sqlx`.

The crate carries its own schema as `sqlx` migrations and puts itself through
`nitinol-store-conformance`, the executable form of the store contracts — the
`EventStore` OCC clauses and the `CheckpointStore` clauses, each against a
schema of its own.

## Using it

```toml
[dependencies]
nitinol = { version = "0.4", features = ["postgres"] }
# or, without the umbrella:
# nitinol-sqlx = "0.1"
```

```rust,ignore
use std::sync::Arc;

use nitinol::persistence::store::EventStore;
use nitinol::sqlx::sqlx::postgres::PgPoolOptions;
use nitinol::sqlx::PostgresEventStore;

let pool = PgPoolOptions::new().connect(&std::env::var("DATABASE_URL")?).await?;
PostgresEventStore::migrate(&pool).await?;

let store: Arc<dyn EventStore> = Arc::new(PostgresEventStore::new(pool)?);
```

Four things the store expects of its caller.

**The pool is yours.** How many connections it holds, and how it authenticates,
are decisions this crate does not make. The concurrency the store can offer is
the concurrency the pool allows.

**Migrations are applied explicitly.** `PostgresEventStore::migrate` brings the
database up to the schema the binary was built against. Nothing runs it for you.

**Every event type is collected at link time, not registered by hand.** Each
component of an `EventType` is a `&'static str`, so a path read out of a column
cannot become one again without leaking the string it was read into.
`#[derive(Event)]` places what your type is written down as in
`nitinol::persistence::EVENT_REGISTRY`, and the framework places its own
records — a saga's outbox, timers and dead letters — the same way. The registry
a load is served out of is built once for the process, out of what the binary
linked; there is none to construct and none to hand over.
`PostgresEventStore::new` refuses to open at all when two declarations claim one
canonical path — though a collision that reaches it is one the build already
had a chance to refuse, since a declaration carries a symbol named after its
canonical path and a second claim on that path is a duplicate definition. A row
whose type nothing collected fails its load rather than being passed over, and a
projector that never sees a row never learns it is there.

A crate the binary references nothing of may be dropped from the link, and its
declarations go with it silently. Where you depend on a crate purely for the
events it declares, reference something of it.

**A stream key is unique across the whole store.** Aggregates and sagas are
journalled side by side in one table, keyed by nothing but the string you append
under. Two of them sharing a key share a stream: their sequence numbers collide,
and each refuses the other's appends.

### TLS

No TLS feature of `sqlx` is enabled: the roots and backends those features reach
for carry licences outside the set this workspace allows. Connecting to a
managed Postgres that requires TLS is not supported by this crate as it stands.

## Projecting with `ExactlyOnce`

A read model that counts — anything an overwrite cannot absorb — needs the
update and the checkpoint that covers it to be one commit. `PostgresCheckpointStore`
is the half of that this crate owns: it keeps a projection's checkpoint in the
same database as the journal, and its `Tx` is a `sqlx` transaction, so a
projector can write both inside one.

**The table comes with the migration you already run.** `PostgresEventStore::migrate`
applies the crate's whole schema, the checkpoint table included. There is no
second migration step to forget.

**The transaction is yours.** This crate implements no `TxProvider`. That trait
belongs to `nitinol-eventsource`, and a provider that depended on the
interpreter in order to implement it would invert the layering this workspace
keeps. Over a `PgPool` it is `begin`, `commit` and `rollback` and nothing
besides.

`TxProvider` lives behind the umbrella's `eventsource` feature, which
`postgres` does not turn on by itself, and implementing it needs `async_trait`
as its own dependency — this crate does not re-export it:

```toml
[dependencies]
nitinol = { version = "0.4", features = ["postgres", "eventsource"] }
async-trait = "0.1"
```

```rust,ignore
use async_trait::async_trait;
use nitinol::eventsource::TxProvider;
use nitinol::sqlx::sqlx::{self, PgPool, PgTransaction};

struct PoolTransactions {
    pool: PgPool,
}

#[async_trait]
impl TxProvider for PoolTransactions {
    type Tx = PgTransaction<'static>;
    type Error = sqlx::Error;

    async fn begin(&self) -> Result<Self::Tx, Self::Error> {
        self.pool.begin().await
    }

    async fn commit(&self, tx: Self::Tx) -> Result<(), Self::Error> {
        tx.commit().await
    }

    async fn rollback(&self, tx: Self::Tx) {
        // A rollback that could not be sent means the connection is going away,
        // and the transaction it held goes with it.
        let _ = tx.rollback().await;
    }
}
```

Hand it to the projector alongside the checkpoint store, and write the read
model through the transaction `project` is given:

```rust,ignore
let checkpoints = Arc::new(PostgresCheckpointStore::new(pool.clone()));

ProjectorProps::new(ProjectionId::new("orders.totals"), store, checkpoints, OrderTotals::new)
    .with_tx_provider(PoolTransactions { pool: pool.clone() })
    .with_event::<OrderPlaced>()
    .catchup_from_global()
    .spawn(&system)
    .await;
```

```rust,ignore
async fn project(
    &mut self,
    event: OrderPlaced,
    ctx: &mut ProjectionContext<'_, PgTransaction<'static>>,
) -> Result<(), Self::Error> {
    // Writing anywhere but through this transaction is what the mode exists to
    // stop: an update on a connection of its own does not roll back with the
    // checkpoint.
    let tx = ctx.tx().ok_or(ProjectionFailed::NoTransaction)?;
    sqlx::query("UPDATE order_totals SET placed = placed + 1")
        .execute(&mut **tx)
        .await?;
    Ok(())
}
```

`with_tx_provider` requires the checkpoint store's `Tx` to be the provider's, so
a pairing that would not share a transaction does not compile. The framework
then drives `begin → project → checkpoint save → commit` for each event, and
rolls back — leaving the event to be redelivered — if any of the four fails.

### A checkpoint moves forward or not at all

A save carrying a sequence the projection has already reached, or one behind it,
is refused as `CheckpointConflict`. It is never absorbed as a successful no-op
and never folded away with a `GREATEST`. The comparison and the write are one
statement against one row, so two writers racing for one projection cannot both
be told they won — including the race to write a projection's *first*
checkpoint, where there is no row yet to compare against.

Under `ExactlyOnce` that refusal is what rolls the read-model update back. A
second writer on a projection meant to have one consumer becomes an error
somebody can see, rather than a counter quietly incremented twice.

A save that skips numbers is still taken: a projection following the store-wide
order moves its checkpoint over events it has no handler for, so the rule is
that the checkpoint moves *forward*, not that it moves by one.

### Telling a conflict from a failure worth retrying

Both reach you as `CheckpointError::Backend`, and they call for different
answers. A `CheckpointConflict` is a premise that was broken: retrying reaches
the same refusal, and what has to change is the deployment. Anything else — a
connection that dropped, a database that was not there — arrives as the `sqlx`
error that raised it, and a retry may well clear it.

```rust,ignore
let CheckpointError::Backend(cause) = failure;
if cause.downcast_ref::<CheckpointConflict>().is_some() {
    // Another writer holds this projection. Retrying will not help.
} else {
    // The database was not reachable. This one is worth retrying.
}
```

### What this guarantees, and what it does not

`ExactlyOnce` is guaranteed for **one consumer per `projection_id`**. Run a
projection from one place.

The refusal above is a defence that makes a broken premise visible — it detects
a duplicate and a checkpoint going backwards. It is not a general answer to
running one projection from two places. A projector loads its checkpoint once at
startup and carries it in memory, and a sequence genuinely past the stored
checkpoint is taken from whoever saves it first. Acquiring and revalidating the
right to process a projection, and ordering events between concurrent
projectors, are not things this crate does.

## Running the tests

The contract tests need a database, and they find one in one of three ways:

1. `DATABASE_URL` names one, and that one is used.
2. There is no `DATABASE_URL`, but a container runtime is reachable: each test
   starts a Postgres of its own and removes it on the way out, panics included.
3. Neither is available, and the tests return without judging the provider.

Only the third leaves a test passing on no evidence, and a run of that shape is
not evidence that this provider keeps the contract. Once a database has been
reached, failing to connect to it, to migrate it, or to keep a clause is a test
failure.

```console
$ cargo test -p nitinol-sqlx
```

### With a container runtime

Nothing to set up beyond a reachable Docker socket — the tests pull the pinned
Postgres image and start it themselves. The socket is looked for at
`$DOCKER_HOST`, then `/var/run/docker.sock`, then the usual rootless locations.

### With a database of your own

Point `DATABASE_URL` at it. This is the path CI takes when it runs Postgres as a
service container, and the path to take when the container runtime is out of
reach or when you want the data to still be there afterwards.

```console
$ DATABASE_URL=postgres://postgres:postgres@localhost:5432/postgres cargo test -p nitinol-sqlx
```

A compose file for a database you keep between runs:

```yaml
services:
  postgres:
    image: postgres:17-alpine@sha256:18cfe3ef5e6815560c98237d6216d1e5119702fb0f3894c8785dd58b8bbe5d73
    environment:
      POSTGRES_PASSWORD: postgres
    ports:
      - "5432:5432"
    healthcheck:
      test: ["CMD", "pg_isready", "-U", "postgres"]
      interval: 1s
      retries: 30
```

```console
$ docker compose up -d
$ DATABASE_URL=postgres://postgres:postgres@localhost:5432/postgres cargo test -p nitinol-sqlx
```

Each test brings up a schema of its own — `nitinol_sqlx_<pid>_<n>` — and
migrates it, so tests running in parallel do not read each other's rows. On the
container path they go when the container does. Against a database of your own
they stay, which is what makes a failure inspectable afterwards; drop them when
you are done:

```sql
DO $$
DECLARE leftover text;
BEGIN
    FOR leftover IN
        SELECT nspname FROM pg_namespace WHERE nspname LIKE 'nitinol\_sqlx\_%'
    LOOP
        EXECUTE format('DROP SCHEMA %I CASCADE', leftover);
    END LOOP;
END $$;
```

### Pinned versions

`testcontainers` and `testcontainers-modules` are pinned exactly rather than by
caret, and the Postgres image by digest. Both crates break their API across
patch ranges of their dependency on each other, and an image tag that moved
would change what a green run means with nothing in the repository having
changed. Moving either is a deliberate change, not something a `cargo update`
should do.
