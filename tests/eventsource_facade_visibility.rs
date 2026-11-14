//! Compile-failure contract for the umbrella's `eventsource` facade, checked
//! with `trybuild`.
//!
//! - `tests/ui/fail/*.rs` — imports that must not resolve through
//!   `nitinol::eventsource`: `SystemEventDecodeError` is framework-internal and
//!   stays out of the `error` facade, so naming it there is an unresolved
//!   import.
//!
//! Without `eventsource` the fixtures would fail for a different reason — the
//! module itself is absent — so the check only runs with that feature.
//!
//! Run with: `cargo test -p nitinol --features eventsource --test eventsource_facade_visibility`.
#![cfg(feature = "eventsource")]

#[test]
fn ui() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/ui/fail/*.rs");
}
