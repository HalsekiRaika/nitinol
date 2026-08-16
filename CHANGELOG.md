# Changelog

All notable changes to this workspace are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

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
