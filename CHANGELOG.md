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
