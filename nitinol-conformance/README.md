# nitinol-conformance

An executable conformance suite for the laws any interpreter of the `nitinol`
contract owes.

A `Decision` says what happened; it does not say who writes it down. Anything
that reads one and carries it out — the event-sourced runtime, a test harness,
an executor that does not exist yet — is an *interpreter*. The laws stated in
the documentation of `nitinol-contract` are what make any two correct
interpreters observationally equivalent. This crate turns them into a suite you
can run against yours:

```rust,ignore
#[tokio::test]
async fn my_executor_conforms() {
    nitinol_conformance::verify(&MyMachinery).await;
}
```

`verify` supplies the domain, the store and the stream key, and reads the
resulting stream back and decodes it itself — an interpreter is never graded on
its own account of what it did. Implementing `Interpretation` and `Interpreter`
is the whole of the wiring: classify each raw outcome into `Interpreted` /
`Unanswered` once, at your own boundary; delegate your codec to
`LedgerEvent::encode` / `LedgerEvent::decode`; and give `quiesce` a real
synchronisation point rather than a wait on a clock.

The crate depends on no interpreter and on no async runtime — neither
`nitinol-eventsource`, `nitinol-runtime` nor `tokio` appears in its dependency
tree — so whichever runtime you await `verify` on is yours to choose.

See [docs.rs](https://docs.rs/nitinol-conformance) for API documentation.
