# Changelog

All notable changes to this workspace are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

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
