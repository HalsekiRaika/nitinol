# nitinol-eventsource

CQRS + Event Sourcing integration layer for the `nitinol` framework. It
interprets the runtime-free contract from `nitinol-contract` (`Aggregate`,
`Decider`, `Decision`, `Query`, `Event`, `Codec`, `Snapshotable`) on top of the
`nitinol-runtime` process model, and adds the execution-side abstractions
(`AggregateProxy`, `Projector`) that drive it.

See [docs.rs](https://docs.rs/nitinol-eventsource) for API documentation.
