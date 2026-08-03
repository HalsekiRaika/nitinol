//! Compile-success / compile-failure contract for `#[derive(Event)]`
//! (`order.md` "テスト: trybuild によるコンパイル成否テスト").
//!
//! - `tests/ui/pass/*.rs`  — forms that must compile (struct, enum, data-bearing
//!   enum, empty family, generics).
//! - `tests/ui/fail/*.rs`  — forms that must be rejected with a `syn::Error`
//!   (missing family attribute, non-string family, union target).
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
