# Benchmarks

What the single-node paths cost, measured with [criterion].

These are a **baseline**, not a target. Nothing here says what any of it ought
to cost: the point is to have a number from before a change so that a number
from after it can be read as a regression, an improvement, or noise. Reading a
conclusion off one of these — that something is too slow, that something needs
optimising — is a separate judgement, and this file does not make it.

criterion is a dev-dependency only, declared once in the workspace's
`[workspace.dependencies]` with `default-features = false` and the features
`async_tokio` and `cargo_bench_support`. No library gained a dependency, and
`plotters` and `rayon` are left out — so **there are no HTML reports**, and what
a run reports is what it prints.

[criterion]: https://bheisler.github.io/criterion.rs/book/index.html

## What there is

| Crate | Bench target | Benchmark | What is timed | N |
|---|---|---|---|---|
| `nitinol-persistence` | `in_memory_event_store` | `in_memory_append/events/N` | the `append` call; each iteration appends under a key of its own into a store holding nothing | 1, 100 |
| `nitinol-persistence` | `in_memory_event_store` | `in_memory_load/by_stream/N` | `load(by_stream)` and pulling every record it serves | 1, 100, 1000 |
| `nitinol-persistence` | `in_memory_event_store` | `in_memory_load/from_global/N` | `load(from_global(1))` and pulling every record it serves | 1, 100, 1000 |
| `nitinol-eventsource` | `event_source_system` | `activation/replay/N` | resolving an identity on a system that has no activation for it, and the `exec` that reads how far the replay got | 1, 100, 1000 |
| `nitinol-eventsource` | `event_source_system` | `activation/snapshot/N` | the same, for an aggregate recovering from a snapshot taken at the stream's own length — so nothing is replayed after it | 1, 100, 1000 |
| `nitinol-eventsource` | `event_source_system` | `projector_catchup/from_aggregate/N` | starting a projector and waiting until its read model has counted the whole stream | 1, 100, 1000 |
| `nitinol-saga` | `saga_round_trip` | `saga_round_trip/poll_1ms` | `ask(PlaceOrder)` → the manager's subscription → the decision it journals → the delivery host → `ask(Reserve)` → the query that sees the reservation | — |
| `nitinol-sqlx` | `postgres` | `postgres_event_store/append/N` | the `append` call, which is one round trip to the database | 1, 100 |
| `nitinol-sqlx` | `postgres` | `postgres_offset_store/save_in_tx` | `pool.begin()`, the save inside that transaction, and the commit | — |
| `nitinol-sqlx` | `postgres_system` | `postgres_activation/replay/N` | `activation/replay/N` over `PostgresStore`: resolving an identity on a system that has no activation for it, and the `exec` that reads how far the replay of N rows got | 1, 100, 1000, 10000 |
| `nitinol-sqlx` | `postgres_system` | `postgres_activation/snapshot/N` | the same, for an aggregate recovering from a snapshot row taken at the stream's own length — so nothing is replayed after it | 1, 100, 1000, 10000 |
| `nitinol-sqlx` | `postgres_system` | `postgres_projection/exactly_once/N` | starting a projector carrying a `TxProvider` over the run's pool, and waiting until its in-memory read model has counted the whole stream — one read and one batch: `begin`, then `project` once per record inside that transaction | 100, 1000 |

What falls outside each timed window — seeding, wiring, building the records an
append is handed — is stated on the benchmark itself, in the rustdoc of the file
it lives in. So is what the iterations of one benchmark do to each other, which
is worth reading before comparing two runs:

| Benchmark | Carried in the number |
|---|---|
| `projector_catchup/*` | **The catch-up itself, and no poll interval when one read takes the whole stream.** A projector's poller reads as soon as it starts, so the framework's default interval of 250 ms — which the benchmark keeps, naming none — is in the window only for a turn after the first, one an interval after the read before it that did not reach the end of the stream. A number from before that first read stopped waiting carries about 250 ms of interval and is not comparable to one from after. |
| `saga_round_trip/poll_1ms` | Both poll intervals — the manager's subscription and the delivery host's — named at one millisecond each, which is what the benchmark's name records. A number taken at another cadence is a different measurement. Every iteration also writes to the one in-memory journal, which this store walks in full on every load, so a run drifts upward as it goes. |
| `activation/*` | Nothing in the window, but an idle activation per iteration stays on the process system: a live aggregate cannot be stopped from outside it. The group's `sample_size` and `measurement_time` are what bound how many. |
| `postgres_event_store/*`, `postgres_offset_store/*` | Rows that nothing removes, so a later iteration writes into a fuller table than an earlier one. |
| `postgres_activation/*` | An idle activation per iteration stays on the process system, as with `activation/*`. The rows read are the ones seeded before the group and no more: an activation only reads. |
| `postgres_projection/exactly_once/*` | One batch: both lengths are within the default batch limit of 1000, so the catch-up is one read and one transaction — its `begin` and N runs of `project` inside it. The window closes when `project` has run for the last record, before the batch's one offset save and commit, so it holds **no commit at all**; that commit is paid while the iteration stops its projector. A number from before the projector settled a batch as one unit holds N − 1 commits and is not comparable to one from after. Each iteration's projection has an identifier of its own and leaves one offset row behind. The poll interval is not in it, for the reason given for `projector_catchup/*`. |

## Running them

```console
$ cargo bench -p nitinol-persistence
$ cargo bench -p nitinol-eventsource
$ cargo bench -p nitinol-saga
$ cargo bench -p nitinol-sqlx
```

One crate at a time rather than `cargo bench --workspace`: `providers/` and the
examples are workspace members outside `default-members`, and the examples carry
no benchmarks at all.

Each crate's benchmarks are sized to finish in well under a minute on the
machine in the table below, which is also what keeps them cheap enough to run
before and after a change rather than once a release.

`nitinol-sqlx` carries two bench targets, and `--bench postgres` or `--bench
postgres_system` runs one of them alone: `postgres` is the store's two write
paths and takes seconds, while most of `postgres_system` is the measurement
time its projection group is given — about 35 s on that machine once the
projector stopped committing per record.

`cargo test --workspace --all-targets` runs every benchmark **once** without
measuring it, which is what catches a benchmark that no longer compiles or no
longer completes. That is a plain part of the test run and needs nothing added
to the command.

### The provider's benchmarks need a database

They run against the one `DATABASE_URL` names and against nothing else:

```console
$ DATABASE_URL=postgres://postgres:postgres@localhost:5432/postgres cargo bench -p nitinol-sqlx
```

With no `DATABASE_URL`, the run says so on stderr and exits successfully, having
registered no benchmark at all:

```console
$ cargo bench -p nitinol-sqlx
nitinol-sqlx benches: nothing to run against, so nothing was measured (no DATABASE_URL)
nitinol-sqlx benches: nothing to run against, so nothing was measured (no DATABASE_URL)
```

One line per bench target, and each has registered nothing.

There is deliberately no fallback that starts a database. A throwaway container
brought up per run measures that container's cold caches and its host's spare
capacity, and a number like that is not a baseline anything can be compared
against later. The contract tests do start one — judging a clause needs any
database, while measuring needs the *same* database twice.

A database to point it at, kept between runs:

```console
$ docker run -d --name nitinol-bench-pg -p 5432:5432 -e POSTGRES_PASSWORD=postgres postgres:17-alpine
```

`providers/nitinol-sqlx/README.md` has a compose file for the same thing.

Each run creates a schema of its own — `nitinol_sqlx_bench_<pid>` — migrates it,
and drops it on the way out. A **panic** leaves it behind, so a crashed run is
cleaned up by hand:

```console
$ psql "$DATABASE_URL" -c "DROP SCHEMA nitinol_sqlx_bench_<pid> CASCADE"
```

## Comparing two runs

Save a baseline, change something, and compare against it:

```console
$ cargo bench -p nitinol-persistence -- --save-baseline first
$ cargo bench -p nitinol-persistence -- --baseline first
```

The second run prints a `change:` line per benchmark and says whether it reads
as an improvement, a regression, or no change at all.

Arguments after `--` reach criterion directly. They can do that because every
crate carrying benchmarks sets `bench = false` on its `[lib]` target: a library
is otherwise a benchmark target in its own right, run through libtest's harness
rather than criterion's, and that harness would take `--save-baseline` for its
own and reject it. Nothing in these libraries is a benchmark, so there is
nothing lost by leaving the argument to criterion.

Baselines are written under `target/criterion/<benchmark>/<name>/`, which
`.gitignore` keeps out of the repository: a baseline belongs to the machine it
was taken on, so there is nothing to commit and nothing to compare across two
of them.

## Measurements

One row per benchmark per measurement run. The value is criterion's point
estimate for the median, and it is only comparable to another row taken on
the same machine.

### 2026-10-19, `cc87e8a`

| | |
|---|---|
| CPU | AMD Ryzen 7 2700X, 8 cores (`nproc` = 8) |
| OS | NixOS 26.05, Linux 6.18.50 |
| Toolchain | rustc 1.98.1, cargo 1.98.1 |
| Database | PostgreSQL 17.11 (`postgres:17-alpine`), in a container on the same machine, default settings |

| Crate | Benchmark | Median |
|---|---|---|
| `nitinol-persistence` | `in_memory_append/events/1` | 331.05 ns |
| `nitinol-persistence` | `in_memory_append/events/100` | 12.693 µs |
| `nitinol-persistence` | `in_memory_load/by_stream/1` | 165.68 ns |
| `nitinol-persistence` | `in_memory_load/by_stream/100` | 10.069 µs |
| `nitinol-persistence` | `in_memory_load/by_stream/1000` | 95.698 µs |
| `nitinol-persistence` | `in_memory_load/from_global/1` | 151.06 ns |
| `nitinol-persistence` | `in_memory_load/from_global/100` | 9.9895 µs |
| `nitinol-persistence` | `in_memory_load/from_global/1000` | 96.295 µs |
| `nitinol-eventsource` | `activation/replay/1` | 36.857 µs |
| `nitinol-eventsource` | `activation/replay/100` | 46.700 µs |
| `nitinol-eventsource` | `activation/replay/1000` | 153.35 µs |
| `nitinol-eventsource` | `activation/snapshot/1` | 29.939 µs |
| `nitinol-eventsource` | `activation/snapshot/100` | 33.097 µs |
| `nitinol-eventsource` | `activation/snapshot/1000` | 61.659 µs |
| `nitinol-eventsource` | `projector_catchup/from_aggregate/1` | 251.25 ms |
| `nitinol-eventsource` | `projector_catchup/from_aggregate/100` | 251.79 ms |
| `nitinol-eventsource` | `projector_catchup/from_aggregate/1000` | 255.09 ms |
| `nitinol-saga` | `saga_round_trip/poll_1ms` | 2.9496 ms |
| `nitinol-sqlx` | `postgres_event_store/append/1` | 14.085 ms |
| `nitinol-sqlx` | `postgres_event_store/append/100` | 14.127 ms |
| `nitinol-sqlx` | `postgres_offset_store/save_in_tx` | 14.106 ms |

Three things about this set, so that the next person reading it does not mistake
them for properties of the code:

* The catch-up numbers are the 250 ms poll interval and very little else: at
  this commit a projector's poller waited out its interval before its first
  read. They are in here because that is the number a deployment waited, not
  because the catch-up is slow. The first read no longer waits, so a later set
  carries what the table above says instead and is not comparable with these.
* The Postgres numbers are a commit each, against a container writing through an
  overlay filesystem with `fsync` on. They say what this machine's durability
  costs; another database will read differently, and a managed one across a
  network differently again.
* `postgres_event_store/append/100` is barely above `append/1`, which is what
  one round trip carrying a hundred rows versus one row looks like when the
  commit dominates.

### 2026-10-24, `4443f69` + #167

| | |
|---|---|
| CPU | AMD Ryzen 7 2700X, 8 cores (`nproc` = 8) |
| OS | NixOS 26.05, Linux 6.18.50 |
| Toolchain | rustc 1.98.1, cargo 1.98.1 |

| Crate | Benchmark | Median |
|---|---|---|
| `nitinol-eventsource` | `projector_catchup/from_aggregate/1` | 36.009 µs |
| `nitinol-eventsource` | `projector_catchup/from_aggregate/100` | 475.07 µs |
| `nitinol-eventsource` | `projector_catchup/from_aggregate/1000` | 4.5047 ms |

Measured on dev `4443f69` with the #167 changes applied, before they were
merged.

Only the catch-up benchmarks were run again; the rest of the 2026-10-19 set
still stands for this machine. The first read no longer waits out the poll
interval, so these numbers are the catch-up itself, as the table at the top
says, and they grow with the stream's length where the earlier ones did not.

### 2026-10-24, `315d6ab` + #169

| | |
|---|---|
| CPU | AMD Ryzen 7 2700X, 8 cores (`nproc` = 8) |
| OS | NixOS 26.05, Linux 6.18.50 |
| Toolchain | rustc 1.98.1, cargo 1.98.1 |
| Database | PostgreSQL 17.11 (`postgres:17-alpine`), in a container on the same machine, default settings, `fsync` on. The container's root is an overlay filesystem; its data directory is a Docker volume on the host's ext4 |

| Crate | Benchmark | Median |
|---|---|---|
| `nitinol-sqlx` | `postgres_event_store/append/1` | 14.267 ms |
| `nitinol-sqlx` | `postgres_event_store/append/100` | 14.266 ms |
| `nitinol-sqlx` | `postgres_offset_store/save_in_tx` | 14.130 ms |
| `nitinol-sqlx` | `postgres_activation/replay/1` | 356.55 µs |
| `nitinol-sqlx` | `postgres_activation/replay/100` | 523.45 µs |
| `nitinol-sqlx` | `postgres_activation/replay/1000` | 1.5437 ms |
| `nitinol-sqlx` | `postgres_activation/replay/10000` | 12.569 ms |
| `nitinol-sqlx` | `postgres_activation/snapshot/1` | 674.16 µs |
| `nitinol-sqlx` | `postgres_activation/snapshot/100` | 650.79 µs |
| `nitinol-sqlx` | `postgres_activation/snapshot/1000` | 653.22 µs |
| `nitinol-sqlx` | `postgres_activation/snapshot/10000` | 1.5320 ms |
| `nitinol-sqlx` | `postgres_projection/exactly_once/100` | 1.4401 s (69.4 events/s) |
| `nitinol-sqlx` | `postgres_projection/exactly_once/1000` | 18.596 s (53.8 events/s) |

Measured on dev `315d6ab` with the #169 changes applied, before they were
merged, in one pass of
`DATABASE_URL=postgres://postgres:postgres@localhost:55432/postgres cargo bench -p nitinol-sqlx`,
which took 4 min 23 s. Events per second is N over the median; criterion's own
`thrpt` line divides by the mean instead and reads lower.

* **Snapshot recovery is flat up to 1000 and not at 10000.** At 1, 100 and 1000
  it is about 650 µs whatever the length, where the in-memory
  `activation/snapshot/N` grows from 30 to 62 µs. Below 1000 it is *slower*
  than a replay, because it is two queries — the snapshot row, then the journal
  above it — against one. At 10000 its median rose to 1.53 ms, and its 95 %
  interval ran from 0.67 to 3.67 ms; a second run of the snapshot benchmarks
  alone gave 3.66 ms, tightly. What the length changes there is the size of the
  whole journal (22 202 rows by then, against 2 202 at the `1000` step), not the
  rows replayed, which are none. The likely cause is the plan Postgres caches
  for the load statement: it is one prepared statement for every kind of query
  (`$1 IS NULL OR persistence_id = $1`, …, `ORDER BY global_sequence`), and
  `EXPLAIN ANALYZE` over the same 22 202 rows shows its generic plan walking
  the `global_sequence` index and filtering every row out (about 3.6–4.2 ms),
  where a plan built for the actual arguments reads the primary key and takes
  0.05 ms. Which plan each pooled connection held during the run was not
  observed.
* **An `ExactlyOnce` catch-up is about one commit per event, when it is
  fast.** `save_in_tx` — a begin, one save, a commit — is 14.1 ms here, so one
  transaction per event caps a projection near 71 events/s. `exactly_once/100`
  came out at 14.4 ms per event, 69.4 events/s. `exactly_once/1000`'s samples
  fell into two groups: five at 23.5–29.1 s and five at 13.3–13.7 s, about
  13.5 ms per event, and the median of 18.6 s lies between them and describes
  neither. A second run of the projection benchmarks alone gave 13.25 s for
  1000 (two slow samples, then eight at 12.4–15.4 s) but 2.63 s for 100. So the
  same work costs 13 or 25–29 ms an event from one stretch of a run to the
  next, and what moves it was not established by these runs; read a difference
  between two runs of these against that spread.

### 2026-10-29, `7908045` + #168

| | |
|---|---|
| CPU | AMD Ryzen 7 2700X, 8 cores (`nproc` = 8) |
| OS | NixOS 26.05, Linux 6.18.50 |
| Toolchain | rustc 1.98.1, cargo 1.98.1 |
| Database | PostgreSQL 17.11 (`postgres:17-alpine`), in the `nitinol-bench-pg` container on the same machine, default settings |

| Crate | Benchmark | Median | Samples | Before |
|---|---|---|---|---|
| `nitinol-eventsource` | `projector_catchup/from_aggregate/1` | 39.605 µs | 37.6–41.5 µs | 36.009 µs |
| `nitinol-eventsource` | `projector_catchup/from_aggregate/100` | 207.69 µs | 199.7–215.4 µs | 475.07 µs |
| `nitinol-eventsource` | `projector_catchup/from_aggregate/1000` | 1.8238 ms | 1.736–1.887 ms | 4.5047 ms |
| `nitinol-sqlx` | `postgres_projection/exactly_once/100` | 1.4797 ms | 1.401–1.584 ms | 1.4401 s |
| `nitinol-sqlx` | `postgres_projection/exactly_once/1000` | 4.2102 ms | 4.153–4.387 ms | 18.596 s |

Measured on dev `7908045` with the #168 changes applied, before they were
merged, with
`DATABASE_URL=postgres://postgres:postgres@localhost:55432/postgres cargo bench -p nitinol-sqlx --bench postgres_system -- postgres_projection`
and `cargo bench -p nitinol-eventsource -- projector_catchup`. Only these were
run again; the rest of the earlier sets still stands for this machine. "Before"
is the median the 2026-10-24 sets record — `4443f69` + #167 for the catch-up,
`315d6ab` + #169 for the projection — and was not measured again in this run.
"Samples" is the per-iteration time of the fastest and the slowest of
criterion's ten samples.

* **The Postgres numbers no longer hold a commit.** The projector now settles
  what one read returned as one batch, and both lengths are within the default
  limit of 1000, so a catch-up is one transaction. The window closes when the
  last `project` has run, which is before that transaction's one save and
  commit (see the table at the top), so what is left in it is the read, the
  `begin` and N runs of `project` in memory. The commit — about 14 ms here, the
  `save_in_tx` of the 2026-10-24 set — is still paid once per catch-up, outside
  the window. Before, the window held N − 1 of them, which is the difference
  between the two columns: a 1000-record catch-up that paid about 14 s of
  commits pays one. Adding one such commit back to the median gives about
  18 ms for 1000 records against 18.6 s — an estimate, since the commit was not
  timed in this run.
* **No two groups this time.** `exactly_once/1000` had split into samples at
  13.3–13.7 s and at 23.5–29.1 s; its ten samples here fall within 4.15–4.39 ms
  of one another. The slow stretches were in the commits, which this window no
  longer holds, so this says nothing about whether they still happen to the one
  commit a catch-up pays.
* **The in-memory catch-up gained at length.** At 100 and 1000 it is less than
  half what it was. What the change took out of each record's path is an
  offset save and an `ask` from the poller to the projector; how the remaining
  time divides was not profiled. At 1 it reads about 10 % higher, 39.6 µs
  against 36.0 µs, where the work is one record and one save either way.
  Whether that is the batch's own bookkeeping or a difference between the two
  days' runs was not established.

## Profiling one of them

Benchmarks say what something costs. A profiler says where.

### Flame graph

```console
$ cargo install flamegraph
$ CARGO_PROFILE_BENCH_DEBUG=true cargo flamegraph --bench event_source_system -p nitinol-eventsource -- --bench --profile-time 10
```

`--bench` is what tells criterion it is being run as a benchmark rather than as
a test, and `--profile-time 10` is what tells it to iterate for ten seconds and
do no analysis at all — which keeps criterion's own statistics out of the graph.
`CARGO_PROFILE_BENCH_DEBUG=true` asks for the symbols rather than adding a
`[profile.bench]` to the workspace, so nothing about an ordinary run changes.

### tokio-console

For what the runtime's tasks are doing rather than where the CPU went.
`examples/runtime/4.watching` is the worked example: it carries the
`console-subscriber` dependency behind a `console` feature, and the subscriber
needs the unstable tokio cfg:

```console
$ cargo install tokio-console
$ RUSTFLAGS="--cfg tokio_unstable" cargo run -p watching --features console
```

The benchmarks here do not carry that subscriber, and the libraries do not
either: it is a dependency an application opts into, and a benchmark measuring
with a task collector attached would be measuring the collector too.
