# Changelog

All notable changes to this workspace are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

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

  Execution-side abstractions stay where they are: `Context`, `Effect`,
  `Codec`, the effectful `nitinol_eventsource::Decider` and the projection
  layer describe how the runtime drives an aggregate, not what an aggregate is,
  and are not part of this crate.

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

  Nothing moves for existing users. The effectful
  `nitinol_eventsource::Decider` and its `Effect` are untouched and keep their
  users; these traits stand beside them under `nitinol-contract`, and no
  existing crate's API changes.

- (`nitinol-eventsource`, `nitinol-saga`): a system-held default `EventStore`.
  `EventSourceSystemBuilder::with_event_store(store)` binds one store to the
  system, and every spawn entry point then resolves it instead of having each
  call site carry the `Arc`: `spawn_aggregate::<A>(id)` /
  `aggregate_props::<A>(id)` on the aggregate side, and — through the new
  `SagaDefaultStoreExt` — `spawn_saga(saga_id, producer)` for a saga's own
  journal, `system.subscription(&key)` for the upstream it polls, and
  `system.saga_manager_props(subscription, producer)` for both at once on a
  manager. `EventStore` is stream-keyed (`append(key, ..)`,
  `LoadQuery::by_stream`), so one instance holds every aggregate's stream and
  every saga's journal side by side under their own keys, which is what makes a
  single default sufficient rather than a registry of named stores.

  Every default keeps a per-spawn override that takes precedence, so splitting
  streams across store instances stays expressible:
  `spawn_aggregate_with_store` / `aggregate_props_with_store`,
  `spawn_saga_with_store`, `Subscription::stream(&store, &key)` and
  `SagaManagerProps::new(store, producer)`.

  Whether a default exists is a typestate on the system
  (`EventSourceSystem<C, StoreUnset>` / `EventSourceSystem<C, StoreSet>`), in
  the same style as the codec marker: the store-less forms exist only on a
  system that was given one, so relying on a store that was never configured is
  a compile error rather than a failure at the first spawn. The parameter
  defaults to `StoreUnset`, and a system built without `with_event_store` keeps
  exactly the previous surface — `spawn_aggregate(id, store)`,
  `aggregate_props(id, store)` and `SagaSystemExt::spawn_saga(saga_id, store,
  producer)` — so existing wiring compiles unchanged.

- (`nitinol-saga`): `SagaSystemExt` / `Subscription` — spawning a saga from an
  `EventSourceSystem`, symmetrically to `EventSourceSystem::spawn_aggregate`.
  An aggregate spawn already resolved its codec from the system, while a saga
  spawn went straight to `ProcessSystem` and had the caller hand over
  `system.codec::<E>()` twice plus a hand-built `SequenceCursor`.
  `system.spawn_saga(saga_id, store, producer).subscribed_to(subscription)`
  takes those from the system instead: the codec for the saga's own events, the
  codec for the events it subscribes to, and the `ProcessSystem` to spawn into.
  `Subscription::stream(&store, &key)` folds the upstream store and its start
  position into one value and defaults to the beginning of the stream, so a
  saga catches up on what was written before it was spawned;
  `Subscription::with_after(n)` resumes past a position already processed.

  The trait is declared in `nitinol-saga` rather than as an inherent method on
  `EventSourceSystem`, because `nitinol-saga` depends on `nitinol-eventsource`
  and an inherent method would make that crate depend back on this one. The
  subscription requirement stays enforced at compile time: `spawn()` exists only
  after `subscribed_to()`.

  `SagaProps` is unchanged and remains the entry point for a saga that needs
  `with_scheduler`, `with_enqueue_policy`, `with_dead_letter_subscriber`,
  `with_crash_restart_factory` or `with_decode_failure_route`.

- (`nitinol-saga`): `SagaManagerProps` / `SagaManagerProxy` — a saga instance
  manager. Previously one `SagaProps` spawn meant one saga bound to one fixed
  `SagaId`, so running a process-manager instance per correlation id required
  pre-spawning every id, and each instance ran its own `DirectPollerProcess`
  over the shared upstream stream — `M` upstream records across `N` instances
  cost `M × N` decodes. The manager holds a *single* upstream subscription and
  interprets `Saga::correlate` at runtime: an id with no resident instance is
  spawned as a child and replays its own stream before the event that caused
  the spawn is delivered; an id already resident is handed the event directly.
  `SagaManagerProps::with_instance_passivation(after)` stops an instance that
  has been idle for `after`, and a later event for the same id spawns it again
  with its state restored by replay. Every instance-level setting `SagaProps`
  accepts has a counterpart on the manager, which hands it to each instance it
  spawns: `with_scheduler`, `with_enqueue_policy`,
  `with_dead_letter_subscriber`, `with_crash_restart_factory` and
  `with_decode_failure_route`. Without them a manager whose `Saga::correlate` is
  constant (the degenerate single-instance case below) would not reproduce what
  `SagaProps` gives a resident saga: no timer would fire for
  `SagaEffect::schedule`, the dead letters its instances write would have no
  subscriber, the configured DLQ filter would be ignored, a tell left in flight
  across a passivation could not be reconstructed, and a corrupt upstream record
  no `Saga::correlate` can claim would have no owner to be attributed to.

  The manager process itself is spawned as persistent: idleness is a
  per-instance signal that `with_instance_passivation` acts on, whereas a
  system-wide default idle timeout reaching the manager — the sole owner of the
  subscription and the registry, with nothing left to revive it — would silently
  stop the whole fan-out during a quiet upstream.

  Collapsing `N` subscriptions into one moves the cursor decision to the
  manager: it withholds the shared cursor when the addressed instance could not
  settle a record (so the record is redelivered), and advances past a record
  that correlates to no instance, so one unclaimed record cannot starve every
  instance behind it. A record addressed to an instance whose stream already
  carries the durable `Ended` marker is recorded as
  `SagaFailure::EndedSagaReceivedMessage` on that instance's own stream and the
  cursor moves on.

  `SagaProps` is unchanged and remains the way to spawn a saga that owns its
  subscription. A `Saga::correlate` that always answers the same `SagaId`
  reduces the manager to exactly one instance, which is the migration path from
  the resident single-saga wiring.

### Changed

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

- **BREAKING** (`nitinol-saga`): `SagaEffect::with_tells` and
  `SagaEffect::with_schedules` are replaced by `SagaEffect::tell_intent(intent)`
  and `SagaEffect::schedule_spec(spec)`. The two setters were only defined on
  the `Persist` branch and panicked on every other receiver, so whether a call
  was legal depended on the runtime variant the caller happened to hold. Once
  `combine` folds an adjacent `Persist × Persist` junction into one `Persist`,
  attaching a tell or a schedule is composition, and composition is `combine`'s
  job: the new constructors each build their own single-element `Persist`, so
  there is no receiver left whose variant could make an attachment illegal and
  no builder call that panics because of one. They keep the capability the
  setters carried and the typed `tell` / `schedule` builders cannot express — a
  `TellIntent::new` intent with no crash-restart payload, and a `ScheduleSpec`
  whose `payload` bytes are given verbatim rather than serialized from a typed
  message. Callers rewrite `persist(e).with_tells(vec![a, b])` as
  `persist(e).combine(SagaEffect::tell_intent(a)).combine(SagaEffect::tell_intent(b))`,
  and `with_schedules` likewise. Note the semantic change this carries:
  `with_*` *set* the list, so a second call dropped what the first attached,
  whereas `combine` concatenates and keeps both.

- (`nitinol-saga`): the `SagaEffect::persist_all` documentation now states what
  the interpreter does with an empty batch. It claimed an empty vector kept the
  "intent to persist zero events" visible to the interpreter, while a `Persist`
  whose `events`, `tells` and `schedules` are all empty has always taken the
  same no-op path as `SagaEffect::None`. Behaviour is unchanged; what an empty
  vector still buys is structural — the value stays a `Persist` that `combine`
  can fold tells and schedules into. Emptiness of `events` alone was never the
  condition, and is now documented as such: a branch carrying a tell or a
  schedule but no user event is what `SagaEffect::tell` and
  `SagaEffect::schedule` build, and it is always interpreted.

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
  `Saga` trait. `SagaProps::with_subscription` no longer takes a
  `Fn(&SubscribedEvent) -> Option<SagaId>` argument; the new required associated
  function `Saga::correlate(event: &Self::SubscribedEvent) -> Option<SagaId>`
  answers it instead. Deriving a business process instance's identity from an
  event is domain knowledge, not wiring, and it was previously duplicated at
  every spawn site. Implementors add `correlate` to their `impl Saga` — returning
  the instance's own `SagaId` reproduces the common closure — and drop the fourth
  argument from every `with_subscription` call. There is deliberately no default
  implementation: a defaulted `None` would silently discard every upstream event.
  `SagaProps::with_decode_failure_route` is **kept on the builder**: a decode
  failure has no typed event to correlate on, so attributing it is routing rather
  than correlation, and it must stay per-instance so two instances of one saga
  type on a shared upstream stream can attribute the same corrupt record
  differently.

- **BREAKING** (`nitinol-saga`): removed the `Saga::State` associated type. It
  was never read by the crate — a saga's state is the implementor itself, which
  `Saga::apply` mutates through `&mut self`. Implementors delete their
  `type State = ...;` line; no other change is required.

### Removed

- **BREAKING** (`nitinol-saga`): removed `Saga::snapshot`, `Saga::from_snapshot`
  and the `SagaSnapshot` type. Snapshotting was never implemented: `snapshot`
  always returned `None` and the `from_snapshot` default panicked with
  `unimplemented!`, so an unimplemented feature was exposed on the user-facing
  trait. A saga replays from its own event stream. Snapshot support will return
  as a separate opt-in `SagaSnapshotable` extension trait, symmetrical to
  `nitinol_eventsource::Snapshotable`. Implementors that overrode either method
  delete their overrides; callers of `SagaSnapshot` have no replacement until
  that extension trait lands.
