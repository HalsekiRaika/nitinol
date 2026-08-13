# Changelog

All notable changes to this workspace are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

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
