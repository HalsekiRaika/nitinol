# Nitinol

[![CI](https://github.com/HalsekiRaika/nitinol/actions/workflows/ci.yml/badge.svg?branch=master)](https://github.com/HalsekiRaika/nitinol/actions/workflows/ci.yml?query=branch%3Amaster)
[![crates.io](https://img.shields.io/crates/v/nitinol.svg)](https://crates.io/crates/nitinol)
[![docs.rs](https://img.shields.io/docsrs/nitinol)](https://docs.rs/nitinol)

An event-sourcing framework for Rust. It provides aggregates, projectors and
sagas, built on a small process runtime.

## Usage

No features are enabled by default. Add the crate with the features you need:

```toml
[dependencies]
nitinol = { version = "0.6", features = ["eventsource"] }
```

## Feature flags

| Feature | Description |
|---|---|
| [`runtime`](https://docs.rs/nitinol/latest/nitinol/runtime/index.html) | The process runtime. |
| [`persistence`](https://docs.rs/nitinol/latest/nitinol/persistence/index.html) | Persistence abstractions such as the event store and IDs. |
| [`contract`](https://docs.rs/nitinol/latest/nitinol/contract/index.html) | The runtime-free contract layer: the aggregate traits and `#[derive(Event)]`, without Tokio. |
| [`eventsource`](https://docs.rs/nitinol/latest/nitinol/eventsource/index.html) | The event sourcing layer: aggregates and projectors. |
| [`saga`](https://docs.rs/nitinol/latest/nitinol/saga/index.html) | The event-sourced process manager. |
| [`full`](https://docs.rs/nitinol/latest/nitinol/) | All of the above. |
| [`postgres`](https://docs.rs/nitinol/latest/nitinol/sqlx/index.html) | The Postgres storage backend. Not part of `full`. |

The API and the design are documented on [docs.rs](https://docs.rs/nitinol).

## Examples

Runnable examples live under
[`examples/runtime`](https://github.com/HalsekiRaika/nitinol/tree/master/examples/runtime),
[`examples/eventsource`](https://github.com/HalsekiRaika/nitinol/tree/master/examples/eventsource) and
[`examples/saga`](https://github.com/HalsekiRaika/nitinol/tree/master/examples/saga).
Each one is a workspace package; run it from a clone of the repository with
`cargo run -p <package>`, for example:

```sh
cargo run -p eventsource-basic-aggregate
```

## Links

- [API documentation (docs.rs)](https://docs.rs/nitinol)
- [CHANGELOG](https://github.com/HalsekiRaika/nitinol/blob/master/CHANGELOG.md)
- [BENCHMARKS](https://github.com/HalsekiRaika/nitinol/blob/master/BENCHMARKS.md)
- [RELEASING](https://github.com/HalsekiRaika/nitinol/blob/master/RELEASING.md)

## License

Licensed under either of
[MIT](https://github.com/HalsekiRaika/nitinol/blob/master/LICENSE_MIT) or
[Apache-2.0](https://github.com/HalsekiRaika/nitinol/blob/master/LICENSE_APACHE),
at your option.
