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

What falls outside each timed window — seeding, wiring, building the records an
append is handed — is stated on the benchmark itself, in the rustdoc of the file
it lives in. So is what the iterations of one benchmark do to each other, which
is worth reading before comparing two runs:

| Benchmark | Carried in the number |
|---|---|
| `projector_catchup/*` | **About 250 ms of poll interval.** A projector's poller waits out its interval before its first read, that interval is the framework's default of 250 ms, and there is no entry point that changes it. So the three stream lengths read close to one another: what differs between them is the work above that floor. |
| `saga_round_trip/poll_1ms` | Both poll intervals — the manager's subscription and the delivery host's — named at one millisecond each, which is what the benchmark's name records. A number taken at another cadence is a different measurement. Every iteration also writes to the one in-memory journal, which this store walks in full on every load, so a run drifts upward as it goes. |
| `activation/*` | Nothing in the window, but an idle activation per iteration stays on the process system: a live aggregate cannot be stopped from outside it. The group's `sample_size` and `measurement_time` are what bound how many. |
| `postgres_*` | Rows that nothing removes, so a later iteration writes into a fuller table than an earlier one. |

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
```

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

* The catch-up numbers are the 250 ms poll interval and very little else, as
  the table above says. They are in here because that is the number a
  deployment waits, not because the catch-up is slow.
* The Postgres numbers are a commit each, against a container writing through an
  overlay filesystem with `fsync` on. They say what this machine's durability
  costs; another database will read differently, and a managed one across a
  network differently again.
* `postgres_event_store/append/100` is barely above `append/1`, which is what
  one round trip carrying a hundred rows versus one row looks like when the
  commit dominates.

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
