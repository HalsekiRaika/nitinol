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

The same holds across crates: a free function a dependency exports is called
through the module it is exported in — `use nitinol_eventsource::codec;` and
then `codec::encode(..)`.

Test fixtures are not exempt. `nitinol-store-conformance` reaches its fixture
builders as `fixture::fact(..)` and `fixture::observe(..)`, which is what keeps
them distinguishable from the helpers each clause module defines for itself.

These stay unqualified:

- Functions defined in the **same** module as the call site.
- `pub use` re-exports in a `lib.rs`. Those assemble the public API surface;
  they are not call sites.
- Macros — `report_system_event!`, `reserved_event!`, `async_trait`, and so on.
- Self-describing predicates, whose bare name already reads as a question about
  the argument — `reject_reserved_id`, `is_within_reserved_namespace`, and the
  like. Qualifying them is allowed but not required.
- Integration tests under `tests/`, which import the public API entry points
  they exercise.

## Code style: what is a free function at all

The rule above says how a free function is *called*. This one says whether it
should be a free function in the first place.

- **A type's own meaning belongs to the type.** Matching, classifying,
  constructing or converting a single type, where everything the answer rests on
  is inside that type or its trait, is a method or an associated function on it.
  A stored identity is matched with `event_type.matches::<ScheduleEvent>()`,
  a timer handle is built with `ScheduleToken::new(saga_id, name)`, and an error
  family is crossed with `From`. Knowledge written outside its owner gets
  written a second time somewhere else, and the two copies disagree.
- **Arranging and interpreting belongs to a module.** Wiring several parts
  together, deciding a subsystem's default, or reading a protocol is a free
  function called as `module::fn`.

The question that separates them: *when the call site reads `module::fn`, is the
module saying what the function is for, in place of a struct?* If it is —
`consumed::settle_with_position`, `lifecycle::append_ended`,
`scheduler::append_schedule_marker` — it stays a free function.

Four free functions stay free for reasons of their own:

| Function | Why |
|---|---|
| `suppress_log::<M>()` | Autoref specialization: the opt-out is detected by a trick that only works on a free function. |
| `to_bigint` / `from_bigint` | The single way across the SQL numeric boundary, deliberately named as one crossing rather than split between two types. |
| `codec::encode` / `codec::decode` | Not a plain call of the codec: they classify its failure into the framework's own error family, which is what the module owns. |
| `fact` / `observe` in `nitinol-store-conformance` | A test DSL, read as a sentence about the store rather than as an operation on a type. |

[book]: https://doc.rust-lang.org/book/ch07-04-bringing-paths-into-scope-with-the-use-keyword.html
