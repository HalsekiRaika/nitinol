//! Compile-failure contract for this crate's typed entry points, checked with
//! `trybuild`.
//!
//! - `tests/ui/fail/*.rs` — forms that must be rejected by the type checker.
//!   `snapshot_on_never_aggregate`: `AggregateProxy::snapshot` exists only for
//!   an aggregate whose `Recover` is `Latest`, so calling it on a `Recover =
//!   Never` aggregate — one that is `Snapshotable` all the same — does not
//!   compile.
//!
//! Fixed with `trybuild` rather than a `compile_fail` doctest because doctests
//! do not run in the gate.
//!
//! The `.stderr` snapshots under `tests/ui/fail` pin the diagnostic.
//! Regenerate them with
//! `TRYBUILD=overwrite cargo test -p nitinol-eventsource --test ui` and confirm
//! the message says `snapshot` is unavailable on `AggregateProxy<Counter>`
//! because of the `Recover = Latest` bound, not for some other reason.

#[test]
fn ui() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/ui/fail/*.rs");
}
