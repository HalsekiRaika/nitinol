# nitinol-sqlx

A Postgres `EventStore` for `nitinol`, built on `sqlx`.

The crate carries its own schema as `sqlx` migrations and puts itself through
`nitinol-store-conformance`, the executable form of the `EventStore` OCC
contract.

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
use nitinol::sqlx::{EventTypeRegistry, PostgresEventStore};

let pool = PgPoolOptions::new().connect(&std::env::var("DATABASE_URL")?).await?;
PostgresEventStore::migrate(&pool).await?;

let mut event_types = EventTypeRegistry::new();
event_types.register(MyEvent::EVENT_TYPE);

let store: Arc<dyn EventStore> = Arc::new(PostgresEventStore::new(pool, event_types));
```

Four things the store expects of its caller.

**The pool is yours.** How many connections it holds, and how it authenticates,
are decisions this crate does not make. The concurrency the store can offer is
the concurrency the pool allows.

**Migrations are applied explicitly.** `PostgresEventStore::migrate` brings the
database up to the schema the binary was built against. Nothing runs it for you.

**Every event type is registered.** Each component of an `EventType` is a
`&'static str`, so a path read out of a column cannot become one again without
leaking the string it was read into. A row whose type no registry entry answers
for fails its load rather than being passed over, and a projector that never
sees a row never learns it is there.

**A stream key is unique across the whole store.** Aggregates and sagas are
journalled side by side in one table, keyed by nothing but the string you append
under. Two of them sharing a key share a stream: their sequence numbers collide,
and each refuses the other's appends.

### TLS

No TLS feature of `sqlx` is enabled: the roots and backends those features reach
for carry licences outside the set this workspace allows. Connecting to a
managed Postgres that requires TLS is not supported by this crate as it stands.

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
