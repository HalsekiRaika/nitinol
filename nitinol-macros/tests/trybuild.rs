//! Compile-success / compile-failure contract for this crate's derives,
//! checked with `trybuild`.
//!
//! - `tests/ui/pass/*.rs`  — forms that must compile. For `#[derive(Event)]`:
//!   struct, enum, data-bearing enum, empty family, generics, a family adjacent
//!   to the reserved namespace. For `#[derive(SagaSubscribe)]`: an alphabet of
//!   several external sources and one deadline.
//! - `tests/ui/fail/*.rs`  — forms that must be rejected with a `syn::Error`.
//!   For `#[derive(Event)]`: missing family attribute, non-string family, union
//!   target, a family inside the `nitinol` reserved namespace. For
//!   `#[derive(SagaSubscribe)]`: a non-enum target, an alphabet with no external
//!   source, more than one deadline variant, a variant naming no input type, a
//!   generic enum, and one event type declared twice.
//!
//! The `saga_subscribe_*` fixtures name `nitinol::saga::*`, so this crate's
//! dev-dependency on `nitinol` must enable the `saga` feature alongside
//! `eventsource`.
//!
//! Note for the implementation step: the `.stderr` snapshots under `tests/ui/fail`
//! pin the *intended* diagnostic message. If the concrete `syn::Error` wording or
//! span differs, regenerate them with `TRYBUILD=overwrite cargo test -p nitinol-macros`
//! and confirm the resulting messages still describe the same failure.

#[test]
fn ui() {
    let t = trybuild::TestCases::new();
    t.pass("tests/ui/pass/*.rs");
    t.compile_fail("tests/ui/fail/*.rs");
}
