# Changelog

All notable changes to this workspace are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- (`nitinol-persistence`): one supplier hands out the stores a deployment runs
  on, so an operator names each logical destination once instead of handing the
  same store to every wiring point by hand.

  Three responsibilities, kept apart. `provider::Store` is the storage itself,
  as one cloneable handle: it owes the journal, and `provider::SnapshotSlot`,
  `provider::ProjectionCheckpointSlot` and
  `provider::DeliveryCheckpointSlot` are the slots beyond it. A store with no
  resource for a slot does not implement that slot's trait, so the wiring
  needing it does not compile — a store nobody configured cannot be relied on,
  and cannot be filled quietly from somewhere else either.
  `provider::CustomStore` is the assembly a mixture is written down in:
  `CustomStore::builder()`, then `eventstore` for the journal and one of
  `snapshot` / `projection_checkpoints` / `delivery_checkpoints` /
  `checkpoints` per further slot, each existing only while that slot is
  `Unprovided`, and `build` existing only once a journal has been named.
  `checkpoints` names one handle for both checkpoint slots, which is the
  single-table deployment. `store::InMemoryStore` is every slot over the
  in-memory reference stores, and `provider::PersistenceProvider::new(store)`
  is what distributes whichever of them the store holds to the framework's
  wiring.

  The handles themselves are now `Clone`, and cloning one names the same data
  region: `store::InMemoryEventStore`, `store::InMemorySnapshotStore` and
  `store::InMemoryCheckpointStore` share their state, while `Default` still
  constructs a destination of its own. A store that is not a cloneable value of
  its own is named through an `Arc`, which the three store traits now implement
  for `Arc<T>` — the `Tx` of a checkpoint store included, so a projector's
  transaction stays visible through it.

  The clauses a store reproduces are labelled the way the other store contracts
  are and stable in the same sense. `PRV-1`: distribution and duplication are
  idempotent — for every slot held, handles taken twice from one store, from a
  clone of the store handle, and from a supplier over either, reach one set of
  records. `PRV-2`: identity is of the data region rather than of the
  connection, so sharing a pool is neither necessary nor sufficient and the
  pool half is the implementor's obligation rather than a suite's to judge.
  `PRV-3`: a mixture of backends is assembled explicitly or not at all.
  `PRV-4`: a projection's position and a delivery host's progress are two
  slots, independently configured, initialised and recovered — naming both with
  one handle is allowed, and what keeps them apart there is the key each
  consumer renders. `PRV-5`: **a supplier is not a transaction** — one store is
  not one atomic unit, and there is no transaction spanning the journal, the
  snapshots and the checkpoints, nor spanning the two checkpoint slots. The
  atomic units stay the ones stated elsewhere (`OCC-3`, and `CKP-6` driven by a
  projector's transaction provider), and `PRV-5` is stated because the name
  invites the opposite reading. It is the one clause no test fixes, and says so
  with the reason.

> **Note on the entries below.** Several of them refine the saga's outbox push
> delivery and its persistent dead letter queue — `SagaEffect::tell_intent`,
> the `EnqueuePolicy` filter, the crash-restart factory, the DLQ pull API and
> the records behind them. Both mechanisms are **withdrawn** in this same
> release; see the two "withdrawn" entries under *Removed*, which supersede
> every earlier mention of them here. Nothing they describe reaches 0.5.0.

- (`nitinol-store-conformance`): `provider::verify` puts a whole store through
  its clauses, beside the per-store suites. A backend implements
  `provider::ProviderUnderTest` — a `bring_up` handing back a `Store` holding
  nothing, as the other suites take a single store — and the per-slot entry
  points (`verify_event_store`, `verify_snapshot_store`,
  `verify_projection_checkpoints`, `verify_delivery_checkpoints`) each observe
  `PRV-1` for that slot across a repeated take *and* across a clone of the
  store handle, then delegate to the store contract behind it — so a backend
  holding one slot runs that slot's entry point and owes the others nothing.
  The clone path is read rather than left to the `Clone` bound, because a
  `Clone` that constructs fresh state satisfies the bound and still splits the
  deployment in two. `verify_checkpoint_independence` is `PRV-4`, read as "each
  slot keeps the answer filed under its own key" — which is what a single-table
  deployment satisfies. The crate still depends on no async runtime.

- (`nitinol-sqlx`): `PostgresStore::new(pool)` assembles this backend as one
  `Store` — the journal and both checkpoint slots over one database, the two
  checkpoint slots answering with one store over one table. `PostgresEventStore`
  and `PostgresCheckpointStore` are `Clone`, copying the pool handle, so a clone
  reaches the same rows. The snapshot slot is left unimplemented, because this
  crate has no Postgres `SnapshotStore`: wiring that needs snapshots is refused
  while it is compiled, rather than filled from a different backend behind the
  operator's back. The supplier conformance entry points are run against it
  where a database can be reached.

- (`nitinol-saga`): a delivery host carries what a saga decided to the
  aggregates it addressed, as a partitioned pull consumer of the saga's own
  stream.

  `Saga::dispatch` is the declaration: a pure associated function from one of
  the saga's own events to the commands it implies, each with the stream key it
  is addressed to. It is the counterpart of `Saga::apply` — that one says what
  a decision means to the saga, this one what it means for everybody else — and
  it is defaulted, so a saga written before it existed keeps compiling and
  delivers nothing. `SagaDeliveryProps` starts the host that consumes it;
  polling, the cursor, the recorded progress, holding and backing off are the
  framework's, and the receiving aggregate needs nothing of the host's on it.

  The subscription is the declared event type as a store-side prefix together
  with the position finished with, so a host of a saga type crosses every
  instance of it — one stream per instance — and no row of another type is
  fetched to be discarded. Delivery is by `ask`, because the receiver's answer
  is the only thing that says a command was carried out: a refusal is observed
  and held, and a collision with an aggregate that already exists is **not**
  read as success, since what such a collision carries says nothing about which
  operation took the position.

  A record counts as finished only once every delivery it implied has landed,
  and nothing is ever passed over for failing — a transient failure is retried
  with a growing pause, a permanent refusal is held and reported with the
  partition, position, target and classification an operator needs.
  `SagaDeliveryProps::with_partitions` spreads the subscription over
  independent processes keyed by `hash(stream key) % N`, so an instance's
  decisions stay in order in one partition and a hold stops that partition
  alone; each has its own recorded progress.
  `SagaDeliveryProxy::advance_checkpoint` is the way past a hold, and it is
  deliberately a person's to take: it declares that the deliveries passed over
  will not be made. `with_poll_interval` and `with_delivery_id` are the other
  deployment choices; the identifier otherwise derives from the declared event
  type. Delivery is at-least-once with no mode to choose, and the host runs
  beside the existing tell path rather than replacing it.

- (`nitinol-persistence`): `SnapshotStore` and `CheckpointStore` now state
  behavioural clauses of their own, labelled the way `EventStore`'s OCC clauses
  are and stable in the same sense. `SNAP-1` … `SNAP-4`: an aggregate nothing
  was saved for is a condition rather than a failure, one save reads back whole
  — the position and the state it was taken at, together, never a pair no save
  wrote — the save that came last is what a load answers with, and one
  aggregate's snapshot is never another's. `CKP-1` … `CKP-6`: never started is
  not the same as at zero, a saved position is what a load answers with, a move
  forward is taken however far it reaches, a save that is not a move forward is
  the backend's to answer, the rendered key is the whole of what a store is
  told — reserved-namespace keys included — and a save handed a transaction
  belongs to it.

  Each clause names what fixes it, and the two that no suite can fix say so
  with the reason: `CKP-4` because the contract leaves two answers open and
  `CheckpointError` has no variant that would tell them apart, `CKP-6` because
  a kit can make no backend's transaction. Both are fixed by the tests of a
  backend that has one.

  What the clauses fix is each store alone. Settling a projection's result and
  the checkpoint covering it in one transaction is a contract of a store *and*
  the projector driving it, and the trait documentation says so and names where
  that is fixed instead.

- (`nitinol-store-conformance`): the snapshot and checkpoint clauses as suites,
  beside the OCC one. `snapshot::verify` has one entry point and no concurrency
  half, because the framework's only consumer of a snapshot store serialises
  every call through one process; `checkpoint::verify_deterministic`,
  `checkpoint::verify_under_concurrency` and `checkpoint::verify` keep the same
  division the OCC suite does, the concurrent half putting several writers
  under keys of their own at once — which is how a partitioned delivery host
  reaches a store, and the one path that puts a provider's connection pool
  under more than one writer. A provider implements
  `snapshot::SnapshotStoreUnderTest` or `checkpoint::CheckpointStoreUnderTest`
  and keeps the existing conventions: a `bring_up` that hands back a store
  holding nothing, and the caller's own runtime borrowed through `Executor`.
  The crate still depends on no async runtime. The in-memory reference stores
  and the Postgres `CheckpointStore` are run through them.

- (`nitinol-persistence`): `EventStore::load` now states `LOAD-1` — a query's
  conditions are a conjunction, so an event-type prefix and a global position
  named together serve what satisfies both, in ascending global order, and the
  position includes the row it names. `nitinol-store-conformance` runs it
  against any backend, from both directions: a prefix every stored row is
  within still excludes the rows below the position, and a prefix no stored row
  is within serves nothing whatever the position.

- (`nitinol-sqlx`): an index on the journal's event type together with its
  global position (`0003_index_journal_event_type.sql`), which is what a
  consumer of one event type asks against. The prefix condition is now asked as
  a half-open range over the column rather than as a function of it, because a
  function of a column cannot reach an index; the range selects exactly the same
  rows, and `COLLATE "C"` — named both in the statement and on the index — is
  what makes that so, since the adjacency of the separator and the byte above
  it is a fact about bytes rather than about a locale.

- (`nitinol-saga`): a saga records the upstream positions it has consumed on its
  own journal, so de-duplicating its input stops being the user's job.

  `nitinol.saga.consumed_position` is a reserved record naming one upstream
  stream and the highest sequence of it the saga has consumed. One `handle`
  call can reach the store as more than one atomic append — a `CancelSchedule`
  or `End` is a boundary the interpreter never merges across — so the position
  rides only the append that closes the input's interpretation out, and is
  never durable before every append that input required is. The position goes
  last in the batch it does ride, which is what leaves every `tell_id` equal to
  the stream sequence of its own `TellRequested` marker.

  Replay rebuilds those positions from the journal alone — highest per source
  wins, and a row that cannot be read is skipped rather than read as a position
  of its own. A redelivered record at or below the position recorded for its own
  source is then dropped before `Saga::handle`, so a saga whose `handle` is not
  idempotent is covered too. A stream subscription additionally resumes past the
  folded position, which takes restart cost off the length of the upstream
  history; the configured `after` still holds as a floor, and a `Global` cursor
  is left exactly as configured, since a per-source sequence cannot be
  translated into a store-wide ordering.

  Where the guarantee stops is deliberate and stated on `Saga::handle`: an
  input is consumed only once *every* append it required has landed, so a
  decision can be durable while its input is not — the reverse never happens.
  A batch that never landed writes no position (its input is handled again,
  at-least-once), and when the append that never landed comes before the one
  that would have closed the input out, that later append is withheld too,
  closing marker included: `End` never durably terminates a saga around a
  decision that input owes but no store holds. A record is also consumed
  without ever producing a `handle` decision at all: one whose upstream bytes
  fail to decode, one that arrives while an earlier correlated record is
  already draining to a dead letter, and one for which `handle` returns an
  error, are each settled by a dead-letter append that carries the same
  position instead, so none of them is redelivered to `handle` either. The
  dead letter that reports a failed persist batch is the exception: it
  deliberately carries no position, because the batch that would have
  carried one is exactly what never landed. An effect that writes nothing
  else still commits a consumed-position-only append when its input has one
  pending, so a no-op decision is not redelivered either; only the entry
  points with no input to consume — a timer firing, and the tell-failure
  hook — commit no position, because they have none to commit. A manager's
  shared cursor is unchanged — it is shared by every instance it fronts, so no
  one instance's position can speak for it — and each instance still drops what
  it has consumed.

  One consequence is worth stating for callers who leaned on redelivery: work an
  interrupted incarnation left unfinished is no longer re-driven by the upstream
  record arriving again, because that record is consumed. It is re-driven by the
  outbox markers the same append made durable — a tell still pending at a restart
  is re-dispatched from its own marker, which is what the fan-out and
  idempotent-receive examples now demonstrate.

- (`nitinol-runtime`): `ProcessSystem::spawn_resident::<P>()`, which resolves a
  process by its type rather than starting one — the mechanism a crate needs to
  keep a singleton of its own running inside a system.

  A type opts in by implementing `Resident`, whose one method states the `Props`
  it is started from. The first ask starts that lifecycle; every later ask
  through the system, or through any clone of it, hands back a proxy to the same
  process, and callers racing on the first ask converge on one lifecycle rather
  than one each. This is the complement of `spawn`, which stays what it was: a
  lifecycle call that starts a process every time and never resolves a running
  one.

  A resident is persistent, and `spawn_resident` is what makes it so — neither
  the system default idle timeout nor an `IdleTimeout` the declaration set for
  itself will reap it. That the guarantee lives in the call and not in the
  declaration is what lets a resident's author leave idling alone instead of
  having to remember it.

  The runtime supplies the mechanism and no residents: it names no resident type
  of its own, so the crate that declares one keeps its own dependency direction.
  The dead-letter stream and process the runtime already keeps alive are
  unchanged and are not routed through this API.

- (`nitinol-macros`, `nitinol-persistence`): a declaration now claims its
  canonical path with an exported symbol, so two declarations claiming one path
  fail the build instead of reaching a deployment.

  `#[derive(Event)]` writes one symbol per declaring type, named
  `_nitinol_event_` followed by the path itself. Within a crate the second
  declaration is `error: symbol ... is already defined`, reported at the derive
  that wrote it; across crates it is the linker's duplicate-symbol error. The
  encoding is injective — `shop.registry.sub.Leaf` and `shop.registry_sub.Leaf`
  are two symbols, not one — and stays inside the character set a linker
  accepts: a byte that is not ASCII alphanumeric, `_` or `.` is written as `..`
  and two hex digits, which no path produces on its own. The symbol carries the
  path and nothing else, so what a linker error and a debugger have to show
  stays short.

  The registry's own refusal is unchanged and is not redundant: link-time
  detection has holes (static libraries linked together need not report a
  duplicate), and an entry placed by hand rather than derived carries no symbol
  at all. The derive's rustdoc carries both messages and the two layers.

- (`nitinol-saga`): a structured `tracing` record at every point the saga
  writes a framework record of its own, carrying `event_type` (the canonical
  path the row was written under), `arm` (which arm of that declaration the
  record holds, from the family's own fixed set) and `stream_id`, plus the
  context of the write — a tell id, a timer, the sequence a dead letter
  occupies.

  The `event_type` column now names the declaration and nothing finer, so the
  arm a framework record holds is no longer legible from a stored row. Shipping
  the records in the same release as that change is what keeps arm-level
  visibility from having a gap: an operator reads `tell_requested` against
  `tell_failed`, or `scheduled` against `fired`, in the place they already
  watch. Each record is emitted after its append succeeds — one claiming a
  write that never happened would be worse than none.

  A user's own events are not reported. The framework reports what it wrote
  itself, so a deployment's records do not grow with every domain event a saga
  persists; arm-level measurement of a user's events is the user's to add.
  Nothing is added to the dependency tree: `tracing` was already a dependency
  of the crate.

- (`nitinol-conformance`): a conformance suite that makes the laws of the
  contract (L-1 to L-9) executable for *any* interpreter.
  `nitinol_conformance::verify(&my_interpretation).await` drives one clause per
  law and names the law it found broken. Until now those laws were prose in
  `nitinol-contract` plus one-off fixtures inside `nitinol-eventsource`'s own
  test files: anyone writing a second interpreter had nothing to check it
  against, and the laws could only be read, not run.

  The suite supplies the domain — a small `Ledger` with its commands, questions
  and facts — because a suite that let each interpreter bring its own decider
  would grade each one against a different domain. It also supplies the store
  and the stream key, and reads the resulting stream back and decodes it itself,
  so no clause depends on an interpreter agreeing with its own account of what
  it did. Wiring an interpreter up means implementing `Interpretation` and
  `Interpreter`: classify each raw outcome into `Interpreted` / `Unanswered`
  once at your own boundary, and give `quiesce` a real synchronisation point.
  The format needs no wiring at all — `LedgerEvent` names `LedgerCodec` as the
  codec that writes it, so an interpreter reaches the suite's bytes through the
  event type itself.

  The crate reaches no interpreter and no async runtime — neither
  `nitinol-eventsource`, `nitinol-runtime` nor `tokio` is in its dependency
  tree — so whichever runtime `verify` is awaited on is the caller's choice. It
  is a `dev-dependency` of `nitinol-eventsource`, whose aggregate activation now
  passes every clause as part of the ordinary test run, with no feature flag: a
  law that only held under an opt-in configuration would not be a law the crate
  keeps. A second integration test consumes one and the same `Decider`
  implementation through both that activation and an external executor built on
  no runtime at all, and requires the two to leave the domain unable to tell
  them apart — same verdicts, same answers, same facts under the same sequence
  numbers.

  Nothing in `nitinol-contract` changes: the suite is written against its
  existing public surface, and the laws' documentation gained only the name of
  the clause that fixes each one.

- (`nitinol-contract`, `nitinol`): a contract crate holding `Event`,
  `Aggregate` and `Snapshotable`, reachable as `nitinol = { features =
  ["contract"] }`. The three traits are pure — `apply` is a synchronous state
  transition, replay is deterministic, and none of them performs I/O — but they
  were defined in `nitinol-eventsource`, which requires Tokio. A domain-layer
  crate that keeps itself runtime-free therefore could not name the contract it
  is written against. It now can: `nitinol-contract` depends only on
  `nitinol-persistence`, and neither `tokio` nor `nitinol-runtime` appears in
  the `contract` feature's dependency tree.

  Nothing moves for existing users. `nitinol-eventsource` re-exports all three
  traits, so `nitinol::eventsource::{Event, Aggregate, Snapshotable}` and
  `nitinol_eventsource::{...}` keep resolving — to the *same* trait items as
  `nitinol::contract::{...}`, not to forwarding wrappers, so an aggregate
  written against either path satisfies the other's bounds and can be handed to
  `AggregateProps` unchanged.

  `#[derive(Event)]` comes with the feature and now generates
  `impl ::nitinol::contract::Event` instead of `::nitinol::eventsource::Event`.
  The derive previously forced anyone who used it to enable `eventsource`, and
  with it Tokio, purely to make the generated path resolve. Both paths name one
  trait, so derives on the `eventsource` feature are unaffected.

  Execution-side abstractions stay where they are: `Codec`, the aggregate
  activation and the projection layer describe how the runtime drives an
  aggregate, not what an aggregate is, and are not part of this crate.

- (`nitinol-contract`): `Decider<C>`, `Decision<E, O, R>` and `Query<M>` — a
  pure vocabulary for deciding a command and for asking state a question.
  `decide` is synchronous, takes `&self` and returns a value, so "a decision
  performs no I/O" is stated by the type rather than by a comment, and a domain
  crate can property-test its decisions with no async runtime in the dependency
  tree. `Query<M>` is the same for questions: it produces an answer, never an
  event.

  A decision states the facts and the answer together —
  `Decision::persist(events).output(answer)` — or refuses the command with
  `Decision::reject(rejection)`, which carries no events and no output. The
  builder is a typestate: `persist` yields an `Accepting`, and only `output`
  completes a `Decision`, so a decider that states facts but forgets to answer
  does not compile. A command that asks nothing says so once, as
  `type Output = ()`.

  The laws that make any two correct interpreters observationally equivalent
  (L-1 to L-9) are written out in the crate documentation, along with the
  extension rule they follow from: existing contracts are frozen, and new
  meaning arrives as a new trait.

  These are now the framework's only decision and query contracts:
  `nitinol-eventsource` interprets them directly and re-exports them, so
  `nitinol_eventsource::{Decider, Decision, Query}` resolve to the *same* trait
  items, not to forwarding wrappers. `nitinol::contract` re-exports `Decider`,
  `Decision`, `Accepting` and `Query` alongside `Event`/`Aggregate`/
  `Snapshotable`, for the same reason: a `contract`-only, Tokio-free domain
  crate can decide commands and answer queries through `nitinol::contract`
  alone, without enabling `eventsource`. See the Removed entry for the
  effectful `Decider` and `Effect` they replace.

- (`nitinol-eventsource`, `nitinol-saga`): a system-held default `EventStore`.
  `EventSourceSystemBuilder::with_event_store(store)` binds one store to the
  system, and every spawn entry point then resolves it instead of having each
  call site carry the `Arc`: `spawn_aggregate::<A>(id)` /
  `aggregate_props::<A>(id)` on the aggregate side, and — through the new
  `SagaDefaultStoreExt` — `spawn_saga(saga_id, producer)` for a saga's own
  journal, `system.subscription(&key)` for the upstream it polls, and
  `system.saga_manager_props(producer)` for a manager's journal, with each
  observed source bound on the returned builder via
  `.subscribed_to::<E>(system.subscription(&key))`. `EventStore` is
  stream-keyed (`append(key, ..)`,
  `LoadQuery::by_stream`), so one instance holds every aggregate's stream and
  every saga's journal side by side under their own keys, which is what makes a
  single default sufficient rather than a registry of named stores.

  Every default keeps a per-spawn override that takes precedence, so splitting
  streams across store instances stays expressible:
  `spawn_aggregate_with_store` / `aggregate_props_with_store`,
  `spawn_saga_with_store`, `Subscription::stream(&store, &key)` and
  `SagaManagerProps::new(store, producer)`.

  Whether a default exists is a typestate on the system
  (`EventSourceSystem<StoreUnset>` / `EventSourceSystem<StoreSet>`): the
  store-less forms exist only on a system that was given one, so relying on a
  store that was never configured is a compile error rather than a failure at
  the first spawn. The parameter
  defaults to `StoreUnset`, and a system built without `with_event_store` keeps
  exactly the previous surface — `spawn_aggregate(id, store)`,
  `aggregate_props(id, store)` and `SagaSystemExt::spawn_saga(saga_id, store,
  producer)` — so existing wiring compiles unchanged.

  Superseded before release: the store a system holds now comes from a
  persistence supplier, so `with_persistence` replaces `with_event_store` and
  the per-spawn overrides are gone. See the Changed and Removed entries for the
  supplier.

- (`nitinol-saga`): `SagaSystemExt` / `Subscription` — spawning a saga from an
  `EventSourceSystem`, symmetrically to `EventSourceSystem::spawn_aggregate`.
  An aggregate spawn already went through the system, while a saga spawn went
  straight to `ProcessSystem` and had the caller hand over a hand-built
  `SequenceCursor`.
  `system.spawn_saga(saga_id, producer).subscribed_to(subscription)` takes the
  `ProcessSystem` from the system instead, and the saga's journal comes from
  the system's persistence provider.
  `Subscription::stream(&store, &key)` folds the upstream store and its start
  position into one value and defaults to the beginning of the stream, so a
  saga catches up on what was written before it was spawned;
  `Subscription::with_after(n)` resumes past a position already processed.

  The trait is declared in `nitinol-saga` rather than as an inherent method on
  `EventSourceSystem`, because `nitinol-saga` depends on `nitinol-eventsource`
  and an inherent method would make that crate depend back on this one. The
  subscription requirement stays enforced at compile time: `spawn()` exists only
  after `subscribed_to()`.

  `SagaProps` remains the entry point for a saga that needs
  `with_enqueue_policy`, `with_dead_letter_subscriber` or
  `with_crash_restart_factory`. A saga's own timer wiring is resolved from its
  alphabet's `#[subscribe(deadline)]` declaration rather than a builder call,
  and a record whose upstream bytes fail to decode is reported through
  `tracing` and skipped rather than routed to a configurable handler.

- (`nitinol-saga`): `SagaManagerProps` / `SagaManagerProxy` — a saga instance
  manager. Previously one `SagaProps` spawn meant one saga bound to one fixed
  `SagaId`, so running a process-manager instance per correlation id required
  pre-spawning every id, and each instance ran its own `DirectPollerProcess`
  over the shared upstream stream — `M` upstream records across `N` instances
  cost `M × N` decodes. The manager holds one upstream subscription per
  declared source, shared by every instance of the saga type, and
  interprets `Saga::correlate` at runtime: an id with no resident instance is
  spawned as a child and replays its own stream before the event that caused
  the spawn is delivered; an id already resident is handed the event directly.
  `SagaManagerProps::with_instance_passivation(after)` stops an instance that
  has been idle for `after`, and a later event for the same id spawns it again
  with its state restored by replay. Every instance-level setting `SagaProps`
  accepts has a counterpart on the manager, which hands it to each instance it
  spawns: `with_enqueue_policy`, `with_dead_letter_subscriber` and
  `with_crash_restart_factory`. A saga's own timer wiring needs no counterpart
  here: the host resolves the resident timer core from the saga's alphabet, so
  a manager's instances get their deadlines on exactly the same terms a
  standalone saga does. Without the counterparts above a manager whose
  `Saga::correlate` is constant (the degenerate single-instance case below)
  would not reproduce what `SagaProps` gives a resident saga: the dead letters
  its instances write would have no subscriber, the configured DLQ filter
  would be ignored, and a tell left in flight across a passivation could not
  be reconstructed. A record no `Saga::correlate` claims has its source's
  shared cursor advanced past it rather than attributed to any instance.

  The manager process itself is spawned as persistent: idleness is a
  per-instance signal that `with_instance_passivation` acts on, whereas a
  system-wide default idle timeout reaching the manager — the sole owner of
  every source's subscription and the registry, with nothing left to revive
  it — would silently stop the whole fan-out during a quiet upstream.

  Collapsing each source's per-instance subscriptions into one moves that
  source's cursor decision to the manager: it withholds that source's cursor
  when the addressed instance could not settle a record (so the record is
  redelivered), and advances past a record that correlates to no instance, so
  one unclaimed record cannot starve every instance behind it. Each declared
  source keeps its own cursor, independent of every other source's, exactly as
  under a standalone saga. A record addressed to an instance whose stream
  already carries the durable `Ended` marker is recorded as
  `SagaFailure::EndedSagaReceivedMessage` on that instance's own stream and the
  cursor moves on.

  `SagaProps` remains the way to spawn a saga that owns its own subscriptions
  — one per declared source. A `Saga::correlate` that always answers the same
  `SagaId` reduces the manager to exactly one instance, which is the migration
  path from the resident single-saga wiring.

- (`nitinol-persistence`): `EventStore::append` states a fifth OCC clause,
  `OCC-5`: once a reader has observed `global_sequence = N`, no event carrying
  a `global_sequence` at or below `N` becomes newly visible afterwards. This is
  a new obligation on every backend, not a restatement of an existing one — a
  backend that assigns `global_sequence` outside the transaction that commits
  the event (a plain auto-increment column taken before commit is the usual
  way) can let a checkpoint-following reader pass an event that a slower,
  earlier-numbered writer has not committed yet, so the reader never sees it.
  Nothing about `OCC-1` through `OCC-4` catches this, because none of them puts
  a reader and a writer at the same table. `OCC-3`'s note that the
  concurrent-reader visibility clause is not fixed by a test is updated to
  point at the new suite that now fixes it. Per #74, the OCC clauses are part
  of `EventStore`'s documented rustdoc contract, so this addition follows
  semver: an out-of-tree backend that has not been run through the new
  conformance suite is not known to satisfy `OCC-5`, even though it compiles
  and satisfies `OCC-1` through `OCC-4` unchanged.

- (`nitinol-store-conformance`): a conformance suite that verifies any
  `EventStore` implementation against the full `OCC-1`–`OCC-5` contract — the
  store contract's counterpart to what `nitinol-conformance` is for the
  interpreter contract, and to what Akka/Pekko's persistence TCK is for a
  plugin author. `nitinol_store_conformance::verify(&store_under_test,
  &executor).await` runs `verify_deterministic`, which constructs each
  `OCC-1`–`OCC-4` situation itself and needs no executor, followed by
  `verify_under_concurrency`, which spawns concurrent writers through the
  caller-supplied `Executor` against a checkpoint-following reader and
  requires it to observe every committed event exactly once — the one clause
  that needs a reader and writers at the same time. A provider implements
  `StoreUnderTest::bring_up` to hand back an empty store and an `Executor` so
  the writers reach the backend independently of one another; a run whose
  writers queue behind one connection proves nothing about concurrency even if
  it passes.

  The crate depends on no async runtime — neither `tokio` nor
  `nitinol-eventsource` is in its dependency tree — so whichever runtime
  `verify` is awaited on is the caller's choice.

- (`nitinol-eventsource`): an activation whose append is refused because another
  writer has taken its sequence now emits one `tracing` event before it stops,
  under the target `nitinol_eventsource::occ` and the name
  `non_genesis_sequence_conflict`, carrying `aggregate_id` and the `sequence`
  the append was refused at. Losing that race is already handled — the
  activation stops and the reference above it resolves a fresh one — so the
  repair was invisible from the outside and its rate could not be measured,
  which is the measurement that decides whether restricting an aggregate to a
  single activation is worth its cost and whether an aggregate boundary is
  drawing too many writers. Those two values are fixed as part of the crate's
  API, because an operator's filter is written against them.

  Nothing else that ends an append emits it: a refusal on the genesis sequence
  is a creation command arriving twice and is still answered as
  `AskError::AlreadyCreated`, a store that failed to answer says nothing about
  who owns the stream, and a command the decider refused never reached the
  store. No conflict semantics, error type or retryability classification
  changed, and no dependency was added.

  What the event counts is the refusal, not its cause — a second activation of
  the same aggregate and an unrelated writer against the same store are
  indistinguishable here.

### Changed

- **BREAKING** (`nitinol-saga`, `nitinol`): the workspace is **0.5.0**, and a
  saga stream written by 0.4 or earlier cannot be read by it.

  A saga's own stream now holds four kinds of row and no more: a domain event —
  the catch-all — plus the three facts the framework declares for itself,
  `nitinol.saga.schedule`, `nitinol.saga.consumed` and
  `nitinol.saga.lifecycle`. A row inside the reserved namespace matching none of
  those three is a corrupted stream or a version mismatch, and `SagaPersisted`
  refuses it: the load fails rather than reading past it. So every 0.4 stream
  carrying an outbox marker, a dead letter, a disposition marker, or a position
  under the old `nitinol.saga.consumed_position` name is unreadable, and so is
  one carrying only the `Ended` arm of the outbox envelope.

  That refusal is deliberate and there is no way round it. "Recognise it and
  skip it" would mean keeping a reader for a mechanism that no longer exists,
  and a withdrawn record's bytes may well decode cleanly as a domain event — so
  reading past one would quietly fold a bygone mechanism's bookkeeping into the
  saga's state. No migration tool, no skip switch and no quiescing procedure is
  provided; development and test stores are wiped.

- **BREAKING** (`nitinol-saga`): a saga terminates, consumes and refuses
  through the three facts above.

  `Ended` has a declaration of its own, `nitinol.saga.lifecycle`, instead of
  sharing an envelope with a transport mechanism's transitions: "this saga
  terminated" is a fact about the saga's own life, and reading it no longer
  requires knowing that mechanism's grammar. The consumed position is
  `nitinol.saga.consumed`, naming both the upstream stream it was read from and
  the declared source it settled.

  An input `Saga::handle` answers `Err` is now settled by a **position-only
  append** — it was seen, it produced nothing, and that one row is all it
  leaves — with the error itself reaching an operator through `tracing::error!`.
  A record routed to a saga that has already terminated, a deadline firing that
  cannot be read back, and a firing `handle` refuses are each reported and
  dropped with nothing durable written: a terminated saga records no further
  facts, and a firing carries no position to settle.

- **BREAKING** (`nitinol-saga`): replay is a fold and nothing else.

  `process::replay` restores the saga's state and its consumed positions and
  reports the schedules still pending; it decides nothing and writes nothing.
  The branches that decided a pending tell's fate — re-dispatch from a durable
  marker, appending a synthetic `TellFailed`, writing a dead letter mid-replay —
  are gone with the mechanism they served. What a restart still does is
  idempotent reconnection: re-subscribe each declared source from the position
  the journal records for it, and re-arm every unfired deadline.

- (`nitinol-saga`, `nitinol-eventsource`): the delivery guarantee is written
  down on `SagaDeliveryProps` rather than on the withdrawn `SagaEffect::tell`.

  It states where at-least-once comes from — the host's checkpoint moves only
  after every delivery a record implied has landed, and that window is the whole
  of it — what a held partition means and what it costs, and the two layers a
  failure belongs to: transport is the host's to retry, hold and report, while a
  business failure reaches the saga the only way a domain fact can, as an input
  (the answer the addressed aggregate recorded, or the saga's own deadline
  firing when none came). `nitinol-eventsource`'s `R-2` resolve clause links
  there instead of restating a per-stage contract.

- **BREAKING** (`nitinol-eventsource`, `nitinol-saga`): every wiring point
  resolves its stores from a supplier instead of being handed each store at the
  call.

  `EventSourceSystem::builder(ps).with_persistence(PersistenceProvider::new(store)).build()`
  is how a system is built, and `build` exists only once a supplier has been
  supplied, so there is no system whose stores nobody named. The store is the
  system's type parameter, which is what decides the entry points it has:
  `spawn_aggregate(id)` / `aggregate_props(id)` / `aggregate_proxy(id)` need
  only the journal, `snapshot_persistor()` needs the snapshot slot, and
  `projector_props(projection_id, producer)` needs the projection-checkpoint
  slot. A slot the store does not hold is a compile error at the wiring that
  needs it rather than a failure at the first save.
  `system.snapshot_persistor()` hands out **one** persistor process per system,
  shared across clones, because the snapshot contract is written for a single
  consumer serialising its calls; `SnapshotPersistorProxy::pid` is how two
  handles are told to be that one process.

  `SnapshotPersistor::spawn(&ps, &persistence)`,
  `ProjectorProps::new(projection_id, &persistence, producer)` and
  `SagaDeliveryProps::new(&persistence)` take the supplier rather than the
  stores, each resolving the slots it needs — a projector the journal together
  with the projection slot, a delivery host the journal together with the
  delivery slot — so the two halves of a wiring cannot come from two worlds.
  `SagaSystemExt` is now one trait over any store, with
  `spawn_saga(saga_id, producer)`, `subscription(key)` and
  `saga_manager_props(producer)` journalling onto the supplier's store, and
  `SagaDeliveryExt::saga_delivery_props::<S>()` is the delivery host's entry
  point, separate because the delivery slot is one a store need not hold.

  Props taken from a system now carry where they start, so
  `.spawn().await` completes the wiring: `aggregate_props`, `projector_props`,
  `spawn_saga`, `saga_manager_props` and `saga_delivery_props` all hand back
  props in a `Hosted` state whose `spawn` takes no argument. Props built
  directly are `Unhosted` and keep the argument they always took —
  `AggregateProps::new(..).spawn(&ps)`, `SagaProps::new(..).spawn(&ps)`,
  `SagaManagerProps::new(..).spawn(&ps)`,
  `ProjectorProps::new(..).spawn(&ps)` and
  `SagaDeliveryProps::new(..).spawn(&system)` — which replaces the call that
  took a `ProcessSystem` and ignored it. `system::SystemHandle` is the system
  with the store's type erased, and `EventSourceSystem::erase_store` is how a
  caller gets one; only the journal survives the erasure, so a slot nobody
  supplied stays unreachable through it.

  `SagaComponents::system` hands a saga the system hosting it — the same
  process system, the same aggregate registry and the same supplier — so a
  producer that needs a reference to tell resolves it there rather than from a
  clone of the system captured outside the spawn. It answers `Some` for every
  incarnation a system's own entry point started and `None` for one built from
  props on a store, which is the path that has no system to name.

  The low-level entry points that name a store are unchanged and are how a
  second destination is reached: `AggregateProps::new`, `SagaProps::new`,
  `SagaManagerProps::new`, `Subscription::stream` / `Subscription::global` and
  `DeadLetterQueue::new`. Reaching two destinations is therefore something an
  operator writes down — a second supplier, assembled and named as such —
  rather than a per-spawn argument at one call among many.

- **BREAKING** (`nitinol-persistence`): `CheckpointStore::load` and
  `CheckpointStore::save` take `&dyn CheckpointScope` instead of
  `&ProjectionId`. A checkpoint store is the progress cache every event
  consumer shares, and more than one kind of consumer now files progress in it.
  `ProjectionId` implements `CheckpointScope` and renders **verbatim**, so
  stored rows keep the keys they were written under and existing projection
  behaviour is unchanged; `&ProjectionId` still coerces at the call site, so
  callers pass what they already passed. Framework-owned consumers render
  inside the reserved namespace, which every user-facing identifier constructor
  refuses — so the two key spaces cannot collide and the provider's one table
  is shared rather than duplicated. `nitinol-sqlx`'s `CheckpointConflict` names
  the checkpoint key it refused rather than a projection.

- **BREAKING** (`nitinol-saga`, `nitinol-macros`): `Saga::SubscribedEvent` and
  `Saga::ScheduledMessage` / `Saga::on_scheduled` are replaced by one
  associated type, `Saga::Subscribe: SagaSubscribe`, naming the saga's whole
  alphabet of inputs — every external event type it observes, plus its own
  time — instead of splitting them across a single upstream event type and a
  separate timer entry point. A coordinator observes the reply of whoever it
  addressed *and* the passing of its own deadline, which a trait naming only
  one upstream event type could not express.

  A user enum derives the alphabet with `#[derive(SagaSubscribe)]`: one
  newtype variant per external event type, and at most one
  `#[subscribe(deadline)]` variant naming the saga's own time as a
  `DeadlineMessage`. `DeadlineMessage::Codec` self-describes how that message
  is persisted, replacing the removed `ScheduledMessage: Serialize +
  DeserializeOwned` bound — a schedule fact is written through the same `Codec`
  discipline as every other persisted record, not through `serde_json` fixed
  by the trait. `DeadlineMessage::TYPE_NAME` / `DeadlineMessage::CODEC_NAME`
  carry that identity onto the wire itself: the `Scheduled` fact's
  `message_type` / `message_codec` fields, alongside `payload`, so a firing is
  checked against what the fact says rather than decoded under whatever
  alphabet happens to be linked into the reader. A stored identity that
  disagrees with the current declaration is refused (`ReadDeadlineError`)
  instead of being decoded under the wrong `Codec`. `Saga::correlate` and
  `Saga::handle` both `match` on
  `Self::Subscribe`, so a source added to the alphabet without a reaction
  leaves either `match` non-exhaustive at compile time.

  Each external variant is observed as a source of its own — its own
  `subscribed_to::<E>(subscription)` binding, its own cursor, its own runtime
  `DirectPollerProcess` — so one source stalling never stops another from
  progressing; there is deliberately no total order across sources. Binding
  is a typestate the derive generates (`SourceBindings` / `BindSource` /
  `AllSourcesSubscribed`): `spawn` exists only once every declared source has
  been bound, generalizing the existing single-source enforcement
  (`SubscriptionSet` / `SubscriptionUnset`) to an arbitrary alphabet. The
  `#[subscribe(deadline)]` variant is allotted no binding slot — a deadline is
  addressed to the instance that set it and never travels a subscription.

  `Saga::apply` is downgraded to a default no-op, so a stateless translation
  saga no longer writes an empty body to satisfy the trait. Its own event
  stream stays mandatory regardless: the delivery host consumes it and
  consumed positions are folded from it whether or not anything folds into
  in-memory state.

  The saga instance's producer changes from `Fn() -> S` to
  `Fn(SagaComponents<S>) -> S`: capabilities the framework resolves — today,
  `Deadline<M>` for an alphabet that declares `#[subscribe(deadline)]` —
  arrive as an argument instead of being captured by a closure built before
  the resolver that produces them exists. A saga using no capability writes
  `|_| S::default()`.

  Migration: replace `type SubscribedEvent = E;` with a
  `#[derive(SagaSubscribe)] enum` naming `E` as a variant and set `type
  Subscribe = ThatEnum;`. Replace a `type ScheduledMessage` / `on_scheduled`
  pair with a `#[subscribe(deadline)]` variant naming a `DeadlineMessage`
  type, and move timer wiring out of spawn builders (`with_scheduler`,
  removed — see Removed below) into `SagaComponents::deadline::<M>()` inside
  the producer, read and armed through `Deadline::pending` /
  `Deadline::set` / `Deadline::cancel`. Drop an `apply` body that did nothing.
  Update every producer closure to accept `SagaComponents<S>`.

- (`nitinol-saga`): the resident scheduler keeps its deadlines in deadline
  order and waits for the earliest one, instead of turning a hashed timing
  wheel.

  A wheel buys `O(1)` insertion and cancellation for millions of short-lived
  timers, and pays for it with a tick that runs whether or not anything is
  registered, a resolution quantised to that tick, and per-entry bookkeeping
  for any deadline past one revolution. Saga deadlines are the opposite
  regime — few, long-lived, and coarse, since delivery is already paced by the
  subscription's polling interval — so the price bought nothing. What replaces
  it is a `BTreeMap` keyed by deadline and a single `sleep_until` pointed at
  the earliest one: a scheduler holding no deadline arms no sleep and wakes
  never, a deadline hours or days out is just a later key with no revolutions
  to count, and cancelling one is a lookup by name and a removal rather than a
  stale entry filtered out later. Registration is `O(log n)`, which is the
  trade this regime is glad to make.

  A deadline's firing is now addressed to the saga *manager* together with the
  instance's `SagaId`, rather than to a proxy for the incarnation that
  registered it. A deadline outlives the instance that set it — that is what a
  deadline is for — so an instance passivated by
  `SagaManagerProps::with_instance_passivation` is restored from its own
  stream and then handed the firing, where before the firing reached a process
  that had already stopped. Such an instance consequently keeps its deadlines
  armed when it passivates; a standalone `SagaProps` saga, which has no
  manager to restore it, still releases its own as it stops.

  Nothing about where a deadline durably lives changes: the `scheduled` marker
  on the saga's own stream is still the record, written in the same atomic
  batch as the decision that set it, and the scheduler is still a cache in
  front of it that restores nothing of its own after a restart.

- (`nitinol-persistence`, `nitinol-saga`): the framework's own records are
  declared through one macro rather than four hand-written pieces each.

  `nitinol_persistence::reserved_event!` writes the `EventType` constant, the
  registry entry that puts the record in `EVENT_REGISTRY`, the symbol that
  claims its canonical path, and — for a record whose payload is a `oneof` —
  the arm name constants and the `arm()` that reports which one a value holds.
  The saga's outbox, timer, dead-letter and dead-letter-disposition records go
  through it, so the reserved constructors, the `#[distributed_slice]`
  placement, and the hand-written `arm()` match are no longer spelled at each
  declaration — and a framework record can no longer be given an identity
  without being collected under it. Every reserved path and every arm name is
  what it was; no stored row changes. `nitinol-saga` no longer depends on
  `linkme` directly.

- **BREAKING** (`nitinol-persistence`, `nitinol-contract`,
  `nitinol-eventsource`, `nitinol-saga`, `nitinol-macros`, `nitinol`): the
  `event_type` column holds one grammar, and every declaration is written down
  under exactly one path.

  An `EventType` is a `Family` and a `TypeName`, and nothing else. A `TypeName`
  is a single segment by construction, so a canonical path decomposes
  structurally and without ambiguity: the last segment is the type name, and
  every segment before it is the family. Which arm of an enum event a row holds
  is carried by the payload — the codec the declaration names is what reads it
  back — so the column names the declaration and the payload names the case.

  What this fixes is a grammar that had two readings of one string. A row of a
  struct event `C` in family `a.b` and a row of arm `C` of enum `b` in family
  `a` were both written down as `a.b.C`, and nothing in the path said which of
  the two a reader was holding. A registry refused to serve such a pair, so the
  collision surfaced as a build failure rather than as a misrouted row — but it
  surfaced at all only because two declarations could reach one path. They no
  longer can; the collision class is gone from the type level, and the only
  collision left is the one a reader could never disentangle anyway: two types
  declaring one family and one type name between them.

  **Journal migration.** Every row an enum event or a framework record wrote
  carries a path that no declaration answers for any more — `a.b.OrderEvent.
  Placed` where the declaration is now `a.b.OrderEvent`, and
  `nitinol.saga.schedule.scheduled` where the record is now
  `nitinol.saga.schedule`. Such a row fails its load as an unknown event type
  rather than being restored, so an existing journal has to have its
  `event_type` column rewritten to the declaring type's path before it is read
  by this version. The payloads are untouched: which arm a framework record
  holds was already in its `oneof`, and a user's enum arm was already in
  whatever its codec wrote.

  `LoadQuery::by_event_type` is unchanged in shape and narrower in effect: a
  declaration has no descendant path of its own, so it now selects the rows of
  that event type and no others. `by_event_type_prefix` is unchanged — a family
  prefix still selects every declaration beneath it, on segment boundaries.

- **BREAKING** (`nitinol-persistence`, `nitinol-macros`): the `nitinol`
  reserved namespace is refused by `Family::new` and `EventRegistryEntry::new`,
  not by the derive alone.

  Both constructors run inside the initializer of a `const`/`static`, so a
  reserved family is a compile error rather than a condition a running
  deployment discovers — and a hand-written `impl Event` is now bound by the
  same rule the derive already enforced, where before it was documentation. The
  root and the boundary rule stay defined once, beside `Family`;
  `is_within_reserved_namespace` is a `const fn` so both enforcement points can
  read it from there.

  The framework's own records reach the names the framework reserved through
  `nitinol_persistence::__private::{reserved_family,
  reserved_event_registry_entry}`, which waive the namespace rule and nothing
  else: a malformed family is still malformed. `__private` because it is not a
  user surface.

- **BREAKING** (`nitinol-contract`, `nitinol-eventsource`, `nitinol-saga`,
  `nitinol-macros`, `nitinol`): an event names the codec that writes it down,
  and codecs are no longer injected at wiring time.

  `Codec<T>` moves to `nitinol-contract` — `nitinol_eventsource::codec::Codec`
  and `nitinol::contract::Codec` both name it — and two traits gain an
  associated type: `Event::Codec: Codec<Self>` and
  `Snapshotable::Codec: Codec<Self::Snapshot>`. A hand-written impl adds one
  line; a derived one adds one attribute key, `#[event(family = "...", codec =
  MyCodec)]`, which is mandatory for the same reason `family` is — stable Rust
  has no associated-type defaults, so an omitted `codec` would be reported as a
  missing associated type instead of as the attribute its author left out.

  Every injection point is gone with it: `EventSourceSystem::with_codec` and
  the builder's `Unset` stage, `AggregateProps::with_codec` and its
  `CodecSet`/`CodecUnset` typestate, the snapshot codec argument of
  `AggregateProps::with_snapshot_persistor`, the codec argument of
  `ProjectorProps::with_event::<E>`, `SagaProps::with_codec`,
  `SagaManagerProps::with_codec` and their own `CodecSet`/`CodecUnset`, the
  upstream codec argument of `with_subscription`, `EventSourceSystem::codec`,
  and the `type Codec` of `SagaSystemExt` / `SagaDefaultStoreExt`.
  `EventSourceSystem`, `EventSourceSystemBuilder` and `SagaSpawn` each lose a
  type parameter as a result, and `SagaProps` / `SagaManagerProps` are left
  with subscription state alone.

  What the injection bought was the freedom to write one event type in two
  formats, which is not a freedom a stream can survive: a codec chosen at the
  wiring site has to agree with every other site that ever wrote or read that
  stream, and nothing checked that it did. Naming the codec on the event makes
  the agreement a property of the type — two aggregates spawned from one system
  now read and write their own streams in their own formats, and a projector
  subscribing to two event types decodes each with the codec its own type
  named. The type list a projection follows stays explicit, because which
  events it follows is a design statement; what reads them is not.

  `ErasedCodec` and the blanket impl that promoted a `Codec<E>` into it are
  removed — there is nothing left to erase a codec into. Classifying a codec
  failure stays on the interpreter's side: `nitinol_eventsource::codec::encode`
  and `::decode` call a named codec and report its failure as `CodecError`,
  which is where `CodecError::Encode` / `Decode` are still produced.

- **BREAKING** (`nitinol-eventsource`, `nitinol`): `AggregateProxy::ask` answers
  with the decision's output instead of the events it persisted. Its return
  type is `<A as Decider<C>>::Output`, and `AggregateProxy::tell` discards that
  output rather than the event list.

  `ask` returning `Vec<A::Event>` made every caller derive its answer from the
  facts — counting them, matching on them, or ignoring them and asking a query
  straight afterwards — even though the decider had already read exactly those
  facts to reach the answer it never got to state. Worse, the events were the
  aggregate's own record, so returning them made the caller's response shape
  and the stream's contents one thing: an event could not be renamed, split or
  reordered without breaking callers who never wanted to see it. `Decision`
  separates them — `Decision::persist(events).output(answer)` — and `ask`
  delivers the answer, exactly once, while the facts go to the stream.

  A command that has nothing to answer declares `type Output = ()` once on the
  impl, and its callers, which were already discarding the returned `Vec`, are
  unchanged apart from the type. A caller that did read the events either takes
  the answer it actually wanted as `Output`, or reads the stream, which is where
  events were always the authority.

- **BREAKING** (`nitinol-eventsource`, `nitinol`): a creation command that
  collides with an aggregate that already exists is reported as
  `AskError::AlreadyCreated` instead of succeeding with an empty event list.

  A conflict on the genesis sequence still means "already created" and still
  leaves the activation alive and unchanged (C-2 is otherwise intact). What
  changed is what the caller is told. Under the old answer no decision had been
  reached, yet the dispatch reported success — which was expressible only
  because the response was a list of events and the empty list was available to
  stand in for one. With `ask` answering with the decider's `Output`, reporting
  success would mean inventing an answer nobody decided, and a caller would
  believe it created what someone else did. Whether a redelivered creation is a
  duplicate to be ignored, a conflict to be surfaced or a race to be retried is
  the consumer's judgement, and it now sees the collision to make it.
  `AskError::AlreadyCreated.retryability()` is `Permanent`: an aggregate that
  exists will exist on the next attempt too.

- **BREAKING** (`nitinol-eventsource`, `nitinol`): `AskError::Effect` is
  replaced by `AskError::Persist(PersistError)`, and `EffectExecutionError` by
  `PersistError`. The old name covered a `Side` variant that no longer exists;
  what is left — a codec failure and a store append failure — is exactly what
  can go wrong writing an accepted decision down, so the type is named for
  that. `Retryability` is unchanged variant for variant: a sequence conflict and
  a backend failure stay `Transient`, a codec failure stays `Permanent`.

- **BREAKING** (`nitinol-eventsource`, `nitinol-saga`): dispatching a command to
  an aggregate now requires the decider's `Rejection` to be an error value —
  `<A as Decider<C>>::Rejection: std::error::Error + Send + Sync + 'static` on
  `AggregateProxy::ask`, `AggregateProxy::tell`, `AggregateTellTarget::tell`,
  `TellIntent::new`, `TellIntent::new_with_crash_restart` and
  `SagaEffect::tell`. The effectful `Decider` demanded the same bound on the
  associated type itself, so no existing decider is newly excluded; the bound
  has moved to the sites that actually need it. `nitinol_contract::Decider`
  stays free of it, because a decider being property-tested in isolation owes
  nobody a rendered refusal, whereas an interpreter that must return one to a
  caller — or report one that has no caller — cannot do its job without it.

- (`nitinol-eventsource`): an aggregate activation is supervised with
  `SupervisionStrategy::Resume` rather than the runtime default `Stop`.
  An aggregate names its own fatal conditions and signals them itself:
  `stop_self` on an overtaken append (C-1) and on a replay it could not finish.
  Every other outcome a handler reports as an error — a refused command, a
  creation that collided, a store that would not answer — leaves the state
  exactly as it was and says nothing about whether the activation can carry the
  next command. Under `Stop` a single business-rule refusal terminated the
  activation, so a rule that fires normally in production took every later
  command down with it and made a domain refusal indistinguishable from a lost
  stream.

- **BREAKING** (`nitinol-eventsource`): `AggregateTellTarget::aggregate_id_str(&self)
  -> &str` is replaced by `AggregateTellTarget::aggregate_id(&self) ->
  &AggregateId`. The accessor exists so a higher-level consumer (a saga's tell
  intent) can identify a target without a round-trip to the aggregate's
  process, but returning `&str` let a target hand back any string, including
  one that was never a real aggregate id — the id's provenance was lost at the
  trait boundary. Returning the typed `&AggregateId` keeps that provenance:
  the value a target reports is the same `AggregateId` its process was
  addressed by, not a reconstruction. There is no default implementation and
  none is planned — a defaulted body would let an implementor's omission
  compile instead of failing to build, silently reporting whatever the default
  picked. Implementors change their return type and, if they were formatting
  or cloning to produce the old `&str`, return the underlying `AggregateId`
  (or a reference to it) directly; `AggregateProxy`'s implementation already
  does this and needs no changes from its own callers. This is re-exported
  from the umbrella crate as `nitinol::eventsource::AggregateTellTarget`, so
  any out-of-tree implementor of the trait must update at the next compile.

- **BREAKING** (`nitinol-saga`): `SagaEffect::with_tells` is replaced by
  `SagaEffect::tell_intent(intent)`. The setter was only defined on the
  `Persist` branch and panicked on every other receiver, so whether a call was
  legal depended on the runtime variant the caller happened to hold. Once
  `combine` folds an adjacent `Persist × Persist` junction into one `Persist`,
  attaching a tell is composition, and composition is `combine`'s job:
  `tell_intent` builds its own single-element `Persist`, so there is no
  receiver left whose variant could make an attachment illegal and no builder
  call that panics because of one. It keeps the capability the setter carried
  and the typed `tell` builder cannot express — a `TellIntent::new` intent with
  no crash-restart payload. Callers rewrite `persist(e).with_tells(vec![a, b])`
  as `persist(e).combine(SagaEffect::tell_intent(a)).combine(SagaEffect::tell_intent(b))`.
  Note the semantic change this carries: `with_tells` *set* the list, so a
  second call dropped what the first attached, whereas `combine` concatenates
  and keeps both.

  `SagaEffect::with_schedules` had no equally-direct successor on `SagaEffect`
  itself: arming a deadline stopped being something a `handle` body builds by
  naming a `ScheduleSpec` of its own. `ScheduleSpec::new` is `pub(crate)` — the
  only way to obtain one is through `Deadline::set`, reached from the
  `Deadline<M>` component `SagaComponents::deadline` hands out, which already
  returns the `Persist` branch carrying the schedule. Callers that built a
  `ScheduleSpec` by hand move the timer's name, delay and message to a
  `#[subscribe(deadline)]` variant and call `Deadline::set` instead.

- (`nitinol-saga`): the `SagaEffect::persist_all` documentation now states what
  the interpreter does with an empty batch. It claimed an empty vector kept the
  "intent to persist zero events" visible to the interpreter, while a `Persist`
  whose `events`, `tells` and `schedules` are all empty has always taken the
  same no-op path as `SagaEffect::None`. Behaviour is unchanged; what an empty
  vector still buys is structural — the value stays a `Persist` that `combine`
  can fold tells and schedules into. Emptiness of `events` alone was never the
  condition, and is now documented as such: a branch carrying a tell but no
  user event is what `SagaEffect::tell` builds, a branch carrying a schedule
  but no user event is what `Deadline::set` builds, and either is always
  interpreted.

- (`nitinol-saga`): a saga whose stream carries the durable `Ended` marker now
  records what is still routed to it instead of always stopping outright. It
  starts in the drained lifecycle either way, so `Saga::handle` is never
  invoked again; a saga that owns its subscription still stops (nothing can
  reach it once the subscription is declined), while an instance behind a
  `SagaManagerProps` manager stays resident so the manager's deliveries land as
  dead letters on its own stream, and is reaped by passivation. A resident
  instance also starts its DLQ direct poller, so those dead letters reach a
  subscriber registered with `with_dead_letter_subscriber` like any other
  instance's do.

- **BREAKING** (`nitinol-saga`): correlation moved from the spawn wiring to the
  `Saga` trait. Subscribing to a source no longer takes a
  `Fn(&Event) -> Option<SagaId>` argument; the new required associated
  function `Saga::correlate(input: &Self::Subscribe) -> Option<SagaId>`
  answers it instead, once for the saga's whole alphabet rather than once per
  subscribed source. Deriving a business process instance's identity from an
  event is domain knowledge, not wiring, and it was previously duplicated at
  every spawn site. Implementors add `correlate` to their `impl Saga` —
  returning the instance's own `SagaId` reproduces the common closure — and
  drop the correlation closure from every subscription call. There is
  deliberately no default implementation: a defaulted `None` would silently
  discard every upstream event.

  `SagaProps::with_decode_failure_route` is **removed**, not kept on the
  builder: a decode failure has no typed input to correlate on, so there is no
  instance — resident or routed — it could be attributed to; it is reported
  through `tracing` and skipped, and its source's cursor advances past it.

- **BREAKING** (`nitinol-saga`): a saga is started by one verb, `host`, and a
  declared source is bound by one verb, `observing`.

  `SagaSystemExt::host::<S>(factory)` hands back the manager builder, and
  `SagaDeliveryExt::deliver::<S>()` its delivery-host counterpart. The two read
  as the pair they are: one starts the saga, the other starts the host that
  carries out what it decided. Binding a declared source is
  `SagaManagerProps::observing::<E>(Subscription)` on both the hosted builder
  and the store-bound one, so a wiring site says the same word wherever its
  journal comes from.

  There is one verb because there is one thing left to say. Everything the two
  former entry points differed in has been withdrawn over this release — the
  scheduler, the codec stage, the crash-restart factory, the enqueue policy,
  the dead-letter subscriber, the decode-failure route — and what remains at a
  wiring site is the binding of each declared source and the instances'
  passivation, which is one builder's worth. With nothing left to configure
  differently, a second entry point would only have been a second way to say
  the same thing.

  `host` always starts a manager. A saga whose `Saga::correlate` always answers
  the same `SagaId` is that manager with a single instance — the degenerate
  case rather than a separate mechanism — so that is how a process with one
  instance is now run, with no `SagaId` named at the wiring and no difference
  in what the instance does: same replay, same effect interpretation, same
  append ordering, same deadlines. What a deployment *can* still say about an
  instance is how long it stays resident, which is residency rather than
  behaviour.

  Migrating: `system.spawn_saga(id, producer).subscribed_to::<E>(sub)` becomes
  `system.host::<S>(producer).observing::<E>(sub)` and the `SagaId` is deleted
  (`correlate` already answers it); `system.saga_manager_props(producer)`
  becomes `system.host::<S>(producer)`;
  `system.saga_delivery_props::<S>()` becomes `system.deliver::<S>()`;
  `SagaProps::new(id, store, producer)` becomes
  `SagaManagerProps::new(store, producer)`; every `subscribed_to` becomes
  `observing`. A handle is a `SagaManagerProxy<S>` rather than a
  `SagaProxy<S>`, and carries the same `pid` and `stop`.

  One consequence is worth naming on its own, because it is a behaviour change
  and not a rename: a source's cursor is the manager's, so it starts where its
  `Subscription` said rather than being carried forward to what the saga's own
  stream records as consumed. That sweep from the configured position is what
  activates the instances that are not resident, and it costs no second
  decision — each instance still drops the records its own stream names as
  consumed, before `Saga::handle`. A restart therefore re-reads upstream
  history a standalone saga's own cursor used to skip. `Subscription::with_after`
  is still a floor and is still honoured exactly.

- (`nitinol-saga`): `Subscription::with_poll_interval(Duration)` names how
  often one declared source is asked.

  The observing side's counterpart of `SagaDeliveryProps::with_poll_interval`,
  and a deployment's choice for the same reason: a pull consumer trades latency
  for load on the store, and which way a deployment wants that is not something
  the framework can know. It is named per source rather than per saga, because
  the sources of one saga are observed independently and need not be read at
  one cadence — a quiet stream polled rarely beside a busy one polled often.

  A source that names no interval keeps the cadence `nitinol-eventsource`
  already polls at, which stays the sole owner of that default: naming nothing
  is not a way of naming something.

- **BREAKING** (`nitinol-saga`): removed the `Saga::State` associated type. It
  was never read by the crate — a saga's state is the implementor itself, which
  `Saga::apply` mutates through `&mut self`. Implementors delete their
  `type State = ...;` line; no other change is required.

### Removed

- **BREAKING** (`nitinol-saga`): outbox push delivery is withdrawn, with no
  replacement surface of its own.

  Gone: `SagaEffect::tell` and `SagaEffect::tell_intent`, the `tells` field of
  `SagaEffect::Persist`, `TellIntent`, `CrashRestartFactory` with
  `SagaProps::with_crash_restart_factory` and
  `SagaManagerProps::with_crash_restart_factory`, the outbox executor process
  and its appender, `Saga::on_tell_failed`, `SagaContext::failed_tell_ids`, and
  the `nitinol.saga.outbox` declaration with every arm of it.

  What replaces it is the subscription-based delivery host that already ran
  beside it: `Saga::dispatch` is the pure mapping from one of the saga's own
  events to the commands it implies and whom each is addressed to, and
  `SagaDeliveryProps` starts the host that carries them. A saga therefore
  persists decisions and sets its own deadlines, and nothing else — "send this
  command" is no longer expressible from inside `handle`, which is what moves
  the knowledge of delivery (polling, the checkpoint, holding on a failure) out
  of the saga and into one owner.

  The old mechanism persisted a duplicate byte copy of a command as an outbox
  marker and asked the user to hand-write a `CrashRestartFactory` to rebuild it
  after a crash — an unnamed payload plus a user-written restore hook, with no
  precedent in the rest of the framework. Nothing takes its place because
  nothing needs to: the decision is durable before anything is delivered, and
  the host re-derives what follows from it on every read.

  There is deliberately **no** replacement hook for a transport failure.
  Notifying a saga that a command could not be carried out had it reacting to
  the framework's own plumbing; what a saga reacts to is a domain fact — the
  answer, or its own deadline passing.

- **BREAKING** (`nitinol-saga`): the persistent dead letter queue is withdrawn.
  The contract is not redefined elsewhere; it is abolished.

  Gone: `DeadLetterQueue` with `list` / `mark_processed` / `evict`,
  `SagaManagerProxy::dead_letter_queue`, `DeadLetterEvent`, `DeadLetterEntry`,
  `DeadLetterQuery`, `DeadLetterQueueError`, `SagaFailure`, `SourceContext`,
  `EnqueuePolicy`, `EnqueueDecision`, `SagaProps::with_enqueue_policy` and
  `with_dead_letter_subscriber` with their `SagaManagerProps` counterparts, the
  disposition mechanism, and the `nitinol.saga.dead_letter` /
  `nitinol.saga.dead_letter_disposition` declarations.

  It was a shop of type-less bytes kept for ever, and appending to it was a
  single point of failure on the saga's own startup and subscription path: a
  refused dead-letter append stopped a saga that was otherwise healthy. In its
  place, failure has two layers and each is observable where it belongs — see
  `SagaDeliveryProps` for the host's half, and `Saga::handle` for the saga's.

  `nitinol-runtime`'s `DeadLetterProcess` is a different mechanism — an
  in-memory, best-effort, system-wide signal for undeliverable messages — and
  is untouched.

- **BREAKING** (`nitinol-eventsource`, `nitinol-saga`): removed every entry
  point that named a store at the call, now that one provider supplies them:
  `EventSourceSystemBuilder::with_event_store`, the `StoreSet` / `StoreUnset`
  typestate markers, `EventSourceSystem::event_store`, the store-taking forms
  of `spawn_aggregate` and `aggregate_props`, `spawn_aggregate_with_store`,
  `aggregate_props_with_store`, `SagaDefaultStoreExt` with
  `spawn_saga_with_store`, and the store-taking form of
  `SagaSystemExt::spawn_saga`.

  The replacement is `with_persistence` on the builder and the supplier-resolved
  entry points beside it, so a stream cannot be half-moved: a per-spawn
  override only took effect on the first resolve of an identity, which left two
  activations writing to two worlds with nothing reporting it. Placing one
  aggregate's or one saga's stream in a different destination is now a second
  supplier, named as such, with `AggregateProps::new` or `SagaProps::new` built
  on the store it distributes. A system's own store is reached through
  `system.persistence().event_store()` where a raw handle is what a caller
  needs.

- **BREAKING** (`nitinol-persistence`, `nitinol-eventsource`, `nitinol-saga`,
  `nitinol-sqlx`): removed the supplier that was a trait over assembled `Arc`s,
  now that the store itself carries its identity: the `PersistenceProvider`
  trait and its `SnapshotStoreProvider` / `ProjectionCheckpointProvider` /
  `DeliveryCheckpointProvider` sub-traits, the `Persistence` assembly with
  `with_snapshot_store` / `with_projection_checkpoints` /
  `with_delivery_checkpoints` / `with_checkpoints`, the
  `SnapshotStoreProvided` / `ProjectionCheckpointsProvided` /
  `DeliveryCheckpointsProvided` slot markers, `InMemoryPersistence` with
  `Persistence::in_memory`, `EventSourceSystem::erase_persistence`, and
  `nitinol-sqlx`'s `postgres_persistence` with `PostgresPersistence`.

  The replacement is the `Store` / `CustomStore` / `PersistenceProvider` trio
  in the Added entry: `PersistenceProvider::new(InMemoryStore::default())` for
  the in-memory reference stores,
  `PersistenceProvider::new(PostgresStore::new(pool)?)` for Postgres, and
  `PersistenceProvider::new(CustomStore::builder().eventstore(e)…build())`
  where handles are named one by one. `erase_store` replaces
  `erase_persistence`, and `Unprovided` keeps its name and its meaning.

  Wrapping each store in an `Arc` inside the supplier made identity the
  caller's to establish: nothing stopped a store from being constructed twice
  and nothing said which of the two a wiring point had. A store that is
  `Clone`, with `PRV-1` read across that clone, is where the obligation
  belongs.

- **BREAKING** (`nitinol-saga`): removed `spawn_scheduler`. The resident
  scheduler is started by type, through `ProcessSystem::spawn_resident`, and
  `SchedulerProcess` is exported so a caller can name it.

  A spawn function of its own started a timer core on every call, so wiring
  two sagas meant either threading one proxy through the whole startup or
  silently running two cores. `spawn_resident` keys on the type: however many
  places ask, the first ask starts the one scheduler and every later ask
  resolves it.

  ```rust
  // before
  let scheduler = nitinol_saga::spawn_scheduler(&system).await;
  ```

  There is no caller-facing `after` for that snippet: `SagaProps::with_scheduler`
  and `SagaManagerProps::with_scheduler` are themselves removed, and
  `SchedulerProxy` is `pub(crate)` rather than exported.
  `spawn_resident::<SchedulerProcess>()` is now called by the framework alone,
  at every saga and manager spawn whose alphabet declares a
  `#[subscribe(deadline)]` variant, so a saga author never names
  `SchedulerProxy` and never decides whether to wire one in.

- **BREAKING** (`nitinol-persistence`, `nitinol-sqlx`): removed
  `EventRegistry::from_linked` and `EventRegistry::from_entries` from the
  public API. Building the registry a store serves its loads out of is the
  framework's own business: it is built once for the process, out of what the
  binary linked, and a store refers to it rather than constructing one.
  `EventRegistry::resolve`, `EventRegistryEntry` and `EVENT_REGISTRY` are
  unchanged, and a declaration still places its entry exactly as before.

  A caller that held a registry of its own has nothing to replace it with and
  needs nothing: `PostgresEventStore::new(pool)` takes the same argument and
  still refuses to open when two declarations claim one canonical path.
  `EventRegistryError` is now `Clone`, which is how a store reports a refusal
  it was handed by reference.

  Both names are reachable inside the framework through
  `nitinol_persistence::__private`, where the conformance suites judge the
  validation on its own; nothing there is a user surface or carries a stability
  promise.

- **BREAKING** (`nitinol-persistence`, `nitinol-contract`,
  `nitinol-eventsource`): removed every part of the event identity that named
  an enum arm — `Variant`, `TypeKey`, `EventType::{with_variant, variant,
  type_key}`, `ParsedEventType::variant` with its
  `from_struct_path` / `try_from_enum_arm_path` pair (one
  `ParsedEventType::from_path` replaces both, because a path now has one
  reading), `Event::variant` and `SystemEvent::variant`.

  A row carries the path of the type that wrote it, so there is no second
  identity for an arm to occupy. `#[derive(Event)]` no longer generates a
  `variant()` match and places one registry entry rather than one per arm; a
  hand-written `impl Event` or `SystemEvent` loses the method with it.
  Dispatch that compared `type_key()` — the projector's handler lookup and the
  saga's upstream subscription filter — compares the event type itself, which
  is the same comparison now that an identity has no third component.

  Replacing `event.variant()` at a write site means naming the declaration:
  `<E as Event>::EVENT_TYPE`, which is what `appending_system_event` and both
  persist paths now write into the column.

- **BREAKING** (`nitinol-eventsource`, `nitinol`): removed the effectful
  decision contract — `nitinol_eventsource::Decider`, the `Effect<E>` ADT
  (`None` / `Persist` / `Apply` / `Side` / `Sequence`, with `combine`,
  `flatten`, `tell` and `publish`), the `SideEffect` trait, `SideEffectError`
  and `Context`. The decision path now interprets `nitinol_contract::Decider`,
  which `nitinol-eventsource` re-exports together with `Decision` and
  `Accepting`.

  The old `decide` was `async`, was handed a `&mut Context`, and returned a
  description of machinery rather than a conclusion. Each of those let a
  decision do something a replay cannot reproduce. `async` invited a decider to
  consult the world, so replaying the same events into the same state could
  reach a different decision. `Effect::Sequence` let one command become several
  appends, so a reader could observe a decision's second fact without its first,
  and a crash between them left the stream in a state the decider never
  described. `Effect::Apply` moved state without writing anything down, so the
  next replay silently disagreed with the activation that had run. `Context`
  handed out the runtime's identity and sequence number — coordinates the
  machine assigns when it persists — and a domain that read them was writing
  rules against the machinery's bookkeeping. And there was no reply type at all:
  the caller received the events and was expected to work the answer out again.

  Implementors rewrite

  ```rust
  #[async_trait]
  impl Decider<Increment> for Counter {
      type Rejection = AtCeiling;
      async fn decide(&self, _: Increment, _: &mut Context)
          -> Result<Effect<Incremented>, AtCeiling>
      {
          Ok(Effect::persist(Incremented))
      }
  }
  ```

  as

  ```rust
  impl Decider<Increment> for Counter {
      type Output = u64;
      type Rejection = AtCeiling;
      fn decide(&self, _: Increment) -> Decision<Incremented, u64, AtCeiling> {
          Decision::persist(vec![Incremented]).output(self.value + 1)
      }
  }
  ```

  Variant by variant: `Effect::persist(e)` becomes
  `Decision::persist(vec![e]).output(..)`; `persist_all(v)` becomes
  `Decision::persist(v).output(..)`; `Effect::empty()` becomes
  `Decision::persist(Vec::new()).output(..)`, which is a legitimate acceptance
  that appends nothing (L-3); `Err(rejection)` becomes
  `Decision::reject(rejection)`; and a `Sequence` of several `Persist` branches
  becomes one `Decision::persist` carrying the events in order, which is
  persisted as a single atomic append (L-2).

  `Effect::Apply` has no replacement, and needs none: applying an event without
  persisting it is a plain `Aggregate::apply` call or a fold over events, and it
  was never something an interpreter should have been asked to do on a command's
  behalf.

  `Effect::Side` — including `Effect::tell` and `Effect::publish` — has no
  replacement inside a decision either. Reaching another aggregate belongs to a
  saga: `SagaEffect::tell(target, cmd)` writes an outbox marker in the same
  atomic append as the saga's own record, so the dispatch survives a crash and
  is arbitrated by a stream, where `Effect::Side` was a fire-and-forget future
  that vanished if the activation died between the append and the send, and that
  a duplicate activation performed twice with nothing detecting it.
  `examples/eventsource/5.aggregate-communication` is rewritten as that saga and
  doubles as the migration guide.

  `ctx.aggregate_id()` becomes a domain fact: an aggregate receives its
  identifier through its creation event and owns it in state after `apply`
  (L-9), rather than holding a handle the machinery passed in.
  `ctx.sequence()` has no replacement — it exposed the interpreter's own
  bookkeeping (L-8), and a domain that needs an order or a time states it as its
  own fact in an event.

  No deprecated adapter is provided; this is a single migration.

- **BREAKING** (`nitinol-eventsource`): the aggregate state serialization shape
  changed with the removal of `Context`, since an aggregate that used
  `ctx.aggregate_id()` now carries its identifier in its own state. Existing
  snapshots are invalid and must be discarded; affected aggregates rebuild from
  their events, which are unchanged. No snapshot schema migration is needed or
  offered.

- **BREAKING** (`nitinol-eventsource`, `nitinol`): removed the
  eventsource-resident query contract `nitinol_eventsource::Receive<M>`. The
  query path now interprets `nitinol_contract::Query<M>`, which
  `nitinol-eventsource` re-exports — so `nitinol_eventsource::Query` and
  `nitinol::eventsource::Query` resolve to the *same* trait item as
  `nitinol_contract::Query`, not to a forwarding wrapper.

  `Receive<M>` was `async`, took `&mut Context` and lived in the crate that
  runs an aggregate. A question asked of state produces no events and reaches
  no store, so none of that was ever needed to answer one — but the signature
  made it available, and a contract that names an await point and the runtime's
  identity cannot be implemented by a domain crate that stays runtime-free, nor
  reasoned about as a pure function of state. `Query::query(&self, msg)` is
  synchronous and takes the message alone. It also ends the name collision with
  `nitinol_runtime::process::Receive`, which every call site worked around by
  importing the eventsource trait under an alias.

  `AggregateProxy::exec` keeps its name, its call shape and its `ExecError`
  (`Domain` / `Send`), so callers are unchanged. Implementors rewrite

  ```rust
  #[async_trait]
  impl Receive<GetCount> for Counter {
      type Response = u64;
      type Error = std::convert::Infallible;
      async fn recv(&self, _: GetCount, _: &mut Context) -> Result<u64, Self::Error> {
          Ok(self.value)
      }
  }
  ```

  as

  ```rust
  impl Query<GetCount> for Counter {
      type Response = u64;
      type Error = std::convert::Infallible;
      fn query(&self, _: GetCount) -> Result<u64, Self::Error> {
          Ok(self.value)
      }
  }
  ```

  `Query::Response` and `Query::Error` carry no bounds, so a domain crate may
  answer with a non-`Send` value. The bounds the actor machinery needs to carry
  an answer back — `Response: Send + 'static`, `Error: std::error::Error + Send
  + Sync + 'static` — now sit on `exec` and on the process's query handler,
  where the carrying happens, rather than on the contract. An implementation
  that only ever answers through `exec` sees no difference; `Receive` required
  `Response: Sync` and `exec` does not, so this is a relaxation.

  The decision path made the same move; see the entry below for the effectful
  `Decider`, `Effect` and `Context`.

- **BREAKING** (`nitinol-saga`): removed `Saga::snapshot`, `Saga::from_snapshot`
  and the `SagaSnapshot` type. Snapshotting was never implemented: `snapshot`
  always returned `None` and the `from_snapshot` default panicked with
  `unimplemented!`, so an unimplemented feature was exposed on the user-facing
  trait. A saga replays from its own event stream. Snapshot support will return
  as a separate opt-in `SagaSnapshotable` extension trait, symmetrical to
  `nitinol_eventsource::Snapshotable`. Implementors that overrode either method
  delete their overrides; callers of `SagaSnapshot` have no replacement until
  that extension trait lands.

- **BREAKING** (`nitinol-saga`): the standalone saga spawn path is withdrawn,
  and with it the second entry point into the manager.

  Gone: `SagaSystemExt::spawn_saga`, `SagaSystemExt::saga_manager_props`,
  `SagaDeliveryExt::saga_delivery_props`, `SagaSpawn`, `SagaProps`, `SagaProxy`
  and `SagaManagerProps::subscribed_to`. What replaces every one of them is
  named in the `host` / `observing` / `deliver` entry under *Changed*, which
  also carries the call-by-call migration.

  A standalone spawn was a mechanism for something that was never a second
  kind of saga. "One resident instance" is what a `Saga::correlate` answering a
  single id already means, so the builder, the typestate carrier and the handle
  that served it were a parallel path to a result the manager reaches anyway —
  and the binding typestate was written out three times to keep them in step.
  There is one of each now, and the semantics that mattered are kept where they
  belong: the degenerate case is documented on `SagaManagerProps`, and the
  compile errors that pin binding completeness (a declared source left unbound,
  a partial binding, a double binding, an undeclared source) moved onto
  `host`'s own `spawn`.

  `nitinol-saga` keeps `SagaManagerProps::new(store, producer)` as the explicit
  form for a deployment whose instances journal into a destination of their
  own. `SagaProcess` itself is untouched as the manager's child.

  One crate-private consequence, listed because it withdraws a route rather
  than a name: `ScheduleDelivery::ToTheInstance` is gone with the builder that
  was the only thing to produce it. A deadline firing has one route — addressed
  to the manager with the instance's `SagaId` — and that is the route that
  restores a passivated instance before handing the firing over, which the
  withdrawn one could not do.

### Fixed

- (`nitinol-saga`): a deadline firing is no longer closed out before the
  decision it produced is durable, so a firing whose decision could not be
  persisted is tried again instead of being lost.

  The `fired` fact of a schedule is to a firing what the consumed position is to
  an observed record — the row that says the input is dealt with — but it was
  appended *before* the deadline arm's effect was interpreted, to keep a
  same-name reset ordered as `Scheduled → Fired → (new) Scheduled` on the
  stream. Replay folds a `fired` fact by name, so once it landed the deadline
  was gone from the journal: a decision whose append then failed — a crash, or
  simply the staged retries running out — left the firing settled, the decision
  in no store, and the saga waiting on an input that was never coming. Deadlines
  were the one input delivered at-most-once, and the only way a saga observes
  that an answer never came.

  The firing's effect is now interpreted first, and the `fired` fact is appended
  after it, only when there is something left to close out. It is withheld where
  the effect's append did not land — the schedule fact stays unfired, so a later
  activation re-arms the deadline (at once, its due time being past) and decides
  it again — where the arm re-set the same `TimerName` and that fact landed,
  since replacing a schedule by name already supersedes the old deadline, and
  where the saga terminated on the firing. A same-name reset is judged by the
  schedule fact that actually became durable rather than by what the effect
  asked for, so a reset the store refused does not pass for one that took
  effect.

  Firing is therefore at-least-once: a stop between the decision landing and the
  `fired` fact landing repeats the firing, and a deadline arm has to tolerate
  seeing the same one twice. Nothing is retried while the failing incarnation is
  still alive; the retry is a later activation.

  Three firings that previously vanished are now settled once, matching how the
  upstream path settles a record it cannot decide from: one the deadline arm
  returns `Err` for, one whose schedule fact names a message identity the
  alphabet no longer declares, and one whose effect writes nothing. Each used to
  leave no `fired` fact and so was re-armed and repeated by every later
  activation; each now leaves that fact and nothing else.

- **BREAKING** (`nitinol-eventsource`, `nitinol-persistence`): a projector no
  longer lets its subscription move past an event it failed to apply. Under
  `AtLeastOnce` and `ExactlyOnce`, the delivery engine already declined to
  advance the checkpoint on a failed application, but `ProjectorProcess::recv`
  returned `Ok(())` regardless — its `Receive<LoadedEvent>::Error` was
  `Infallible`, so it had no way to say otherwise — and the `DirectPoller`
  advanced its cursor on that `Ok`. The failed event was then overtaken by the
  next one, which saved a higher checkpoint, and the failed event was never
  redelivered: a restart skipped it too, because it now sat below the
  checkpoint. Events were lost permanently and silently.

  `recv` now reports an unapplied event by returning
  `error::ProjectionNotSettled`, which is what the poller already reads as
  "hold the cursor". The projector is spawned with
  `SupervisionStrategy::Resume` so that reporting a failure does not also stop
  the projector and cascade-stop the poller that owes the retry.

  The consequence is deliberate and worth planning for: under `AtLeastOnce` and
  `ExactlyOnce`, an event that can never succeed — an undecodable payload, a
  record the read model always rejects — now stops that projection rather than
  being dropped from it. It is retried, and warned about, once per poll
  interval, and nothing behind it is applied. `AtMostOnce` is unchanged: it
  saves the checkpoint before the handler and still moves on from a failed one.

  Two cases that previously had no stated answer now have one. `ExactlyOnce`
  **without** a `TxProvider` takes the `Result` of `project()` as the whole
  account of what happened — `Err` holds the cursor — while the framework still
  saves no checkpoint in that mode. A failed `TxProvider::commit` is read as
  **uncommitted**: `commit` consumes the transaction and answers with a single
  `Err`, so a rejected commit and a commit whose reply was lost are
  indistinguishable, and the reading that cannot lose the event is the one
  taken. An `ExactlyOnce` projector therefore degrades to at-least-once across
  an unknown commit.

  `Receive<LoadedEvent>` for `ProjectorProcess` changes its `Error` associated
  type from `std::convert::Infallible` to
  `nitinol_eventsource::error::ProjectionNotSettled`. Code that spawns
  projectors through `ProjectorProps` is unaffected; code that named that
  associated type, or that called `ask` on a projector proxy and matched on the
  resulting error, sees the new type.
