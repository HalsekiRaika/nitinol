//! Compile-failure contract for this crate's typed entry points, checked with
//! `trybuild`.
//!
//! - `tests/ui/fail/*.rs` — forms that must be rejected by the type checker.
//!   `snapshot_on_never_aggregate`: `AggregateProxy::snapshot` exists only for
//!   an aggregate whose `Recover` is `Latest`, so calling it on a `Recover =
//!   Never` aggregate — one that is `Snapshotable` all the same — does not
//!   compile.
//!   `contingent_system_has_no_snapshot_persistor` and
//!   `contingent_system_has_no_persistence`: a `ContingentSystem` does not know
//!   which slots the deployment holds, so it offers neither the typed system's
//!   `snapshot_persistor()` nor its `persistence()`.
//!
//! Fixed with `trybuild` rather than a `compile_fail` doctest because doctests
//! do not run in the gate.
//!
//! The `.stderr` snapshots under `tests/ui/fail` pin the diagnostic.
//! Regenerate them with
//! `TRYBUILD=overwrite cargo test -p nitinol-eventsource --test ui` and confirm
//! the message says `snapshot` is unavailable on `AggregateProxy<Counter>`
//! because of the `Recover = Latest` bound, not for some other reason, and that
//! the `ContingentSystem` fixtures fail with `E0599` (no such method) rather
//! than an unresolved import.

#[test]
fn ui() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/ui/fail/*.rs");
}
