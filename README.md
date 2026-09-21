# Nitinol

An Event Sourcing toolkit for Rust.

## Code style: calling free functions

A free function defined in another module is called through that module, not
imported by name. `use` brings the **module** into scope and the call site reads
`module::function()`. Types, traits, structs and enums are the other way round:
the item itself is imported and used unqualified. This is the convention the
Rust book describes in [Bringing Paths into Scope with the `use` Keyword][book].

```rust,ignore
use crate::process::lifecycle::{self, LifecycleConfig};

lifecycle::run(LifecycleConfig { /* ... */ }).await;
```

The point is that the call site says where the function comes from. That matters
most where the bare name carries no context on its own (`run`, `observe`), or
where it collides with a nearby field, trait method, or local binding.

The same holds across crates: a free function re-exported by a dependency is
called through the path it is exported at, e.g.
`nitinol_eventsource::appending_system_event(..)`.

Test fixtures are not exempt. `nitinol-store-conformance` reaches its fixture
builders as `fixture::fact(..)` and `fixture::observe(..)`, which is what keeps
them distinguishable from the helpers each clause module defines for itself.

These stay unqualified:

- Functions defined in the **same** module as the call site.
- `pub use` re-exports in a `lib.rs`. Those assemble the public API surface;
  they are not call sites.
- Macros — `report_system_event!`, `reserved_event!`, `async_trait`, and so on.
- Self-describing predicates, whose bare name already reads as a question about
  the argument — `is_dead_letter_event_type`, `reject_reserved_id`,
  `is_within_reserved_namespace`, and the like. Qualifying them is allowed but
  not required.
- Integration tests under `tests/`, which import the public API entry points
  they exercise.

[book]: https://doc.rust-lang.org/book/ch07-04-bringing-paths-into-scope-with-the-use-keyword.html
