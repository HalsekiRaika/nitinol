use nitinol_persistence::AggregateId;
use nitinol_saga::{SagaContext, SagaId};

#[test]
fn test_context_exposes_given_saga_id() {
    let saga_id = SagaId::new("ctx-test-saga-id");
    let ctx = SagaContext::test_context(saga_id.clone(), 0);
    assert_eq!(
        ctx.saga_id(),
        &saga_id,
        "test_context must expose the saga_id it was constructed with"
    );
}

#[test]
fn test_context_exposes_given_sequence() {
    let saga_id = SagaId::new("ctx-test-seq");
    let ctx = SagaContext::test_context(saga_id, 17);
    assert_eq!(
        ctx.sequence(),
        17,
        "test_context must expose the saga sequence it was constructed with"
    );
}

#[test]
fn test_context_defaults_are_deterministic_across_calls() {
    let saga_id = SagaId::new("ctx-test-deterministic");
    let a = SagaContext::test_context(saga_id.clone(), 0);
    let b = SagaContext::test_context(saga_id, 0);
    assert_eq!(
        a.upstream_sequence(),
        b.upstream_sequence(),
        "test_context default upstream_sequence must be deterministic"
    );
    assert_eq!(
        a.now(),
        b.now(),
        "test_context default now must be deterministic"
    );
}

#[test]
fn test_context_defaults_upstream_sequence_to_zero() {
    let saga_id = SagaId::new("ctx-test-upstream-seq-default");
    let ctx = SagaContext::test_context(saga_id, 0);
    assert_eq!(
        ctx.upstream_sequence(),
        0,
        "test_context must default upstream_sequence to 0"
    );
}

#[test]
fn test_context_defaults_now_to_unix_epoch() {
    let saga_id = SagaId::new("ctx-test-now-default");
    let ctx = SagaContext::test_context(saga_id, 0);
    assert_eq!(
        ctx.now(),
        jiff::Timestamp::UNIX_EPOCH,
        "test_context must default now() to jiff::Timestamp::UNIX_EPOCH"
    );
}

#[test]
fn with_upstream_sets_upstream_aggregate_id() {
    let saga_id = SagaId::new("ctx-with-upstream-agg-id");
    let upstream = AggregateId::new("upstream-aggregate-x");
    let ctx = SagaContext::test_context(saga_id, 0).with_upstream(upstream.clone(), 0);
    assert_eq!(
        ctx.upstream_aggregate_id(),
        &upstream,
        "with_upstream must set upstream_aggregate_id"
    );
}

#[test]
fn with_upstream_sets_upstream_sequence() {
    let saga_id = SagaId::new("ctx-with-upstream-seq");
    let upstream = AggregateId::new("upstream-aggregate-y");
    let ctx = SagaContext::test_context(saga_id, 0).with_upstream(upstream, 42);
    assert_eq!(
        ctx.upstream_sequence(),
        42,
        "with_upstream must set upstream_sequence"
    );
}

#[test]
fn with_upstream_does_not_mutate_saga_id_or_sequence() {
    let saga_id = SagaId::new("ctx-with-upstream-isolate");
    let upstream = AggregateId::new("upstream-aggregate-z");
    let ctx = SagaContext::test_context(saga_id.clone(), 99).with_upstream(upstream, 7);
    assert_eq!(
        ctx.saga_id(),
        &saga_id,
        "with_upstream must not mutate saga_id"
    );
    assert_eq!(
        ctx.sequence(),
        99,
        "with_upstream must not mutate the saga's own sequence"
    );
}

#[test]
fn with_now_sets_timestamp() {
    let saga_id = SagaId::new("ctx-with-now");
    let ts = jiff::Timestamp::from_second(1_700_000_000)
        .expect("constructing a valid jiff::Timestamp from a second value must succeed");
    let ctx = SagaContext::test_context(saga_id, 0).with_now(ts);
    assert_eq!(ctx.now(), ts, "with_now must set the now() timestamp");
}

#[test]
fn with_now_does_not_mutate_other_fields() {
    let saga_id = SagaId::new("ctx-with-now-isolate");
    let upstream = AggregateId::new("upstream-now-isolate");
    let ts = jiff::Timestamp::from_second(1_650_000_000)
        .expect("constructing a valid jiff::Timestamp from a second value must succeed");
    let ctx = SagaContext::test_context(saga_id.clone(), 5)
        .with_upstream(upstream.clone(), 3)
        .with_now(ts);
    assert_eq!(ctx.saga_id(), &saga_id, "with_now must not mutate saga_id");
    assert_eq!(
        ctx.sequence(),
        5,
        "with_now must not mutate the saga's own sequence"
    );
    assert_eq!(
        ctx.upstream_aggregate_id(),
        &upstream,
        "with_now must not mutate upstream_aggregate_id"
    );
    assert_eq!(
        ctx.upstream_sequence(),
        3,
        "with_now must not mutate upstream_sequence"
    );
}

#[test]
fn builder_chain_composes_all_fields_independently() {
    let saga_id = SagaId::new("ctx-chain");
    let upstream = AggregateId::new("upstream-chain");
    let ts = jiff::Timestamp::from_second(1_555_555_555)
        .expect("constructing a valid jiff::Timestamp from a second value must succeed");
    let ctx = SagaContext::test_context(saga_id.clone(), 11)
        .with_upstream(upstream.clone(), 22)
        .with_now(ts);
    assert_eq!(ctx.saga_id(), &saga_id);
    assert_eq!(ctx.sequence(), 11);
    assert_eq!(ctx.upstream_aggregate_id(), &upstream);
    assert_eq!(ctx.upstream_sequence(), 22);
    assert_eq!(ctx.now(), ts);
}

#[test]
fn with_upstream_called_twice_keeps_last_value() {
    let saga_id = SagaId::new("ctx-upstream-overwrite");
    let first = AggregateId::new("first-upstream");
    let second = AggregateId::new("second-upstream");
    let ctx = SagaContext::test_context(saga_id, 0)
        .with_upstream(first, 1)
        .with_upstream(second.clone(), 99);
    assert_eq!(
        ctx.upstream_aggregate_id(),
        &second,
        "the most recent with_upstream call must win for upstream_aggregate_id"
    );
    assert_eq!(
        ctx.upstream_sequence(),
        99,
        "the most recent with_upstream call must win for upstream_sequence"
    );
}

#[test]
fn with_now_called_twice_keeps_last_value() {
    let saga_id = SagaId::new("ctx-now-overwrite");
    let first = jiff::Timestamp::from_second(1_000_000_000)
        .expect("constructing a valid jiff::Timestamp from a second value must succeed");
    let second = jiff::Timestamp::from_second(2_000_000_000)
        .expect("constructing a valid jiff::Timestamp from a second value must succeed");
    let ctx = SagaContext::test_context(saga_id, 0)
        .with_now(first)
        .with_now(second);
    assert_eq!(
        ctx.now(),
        second,
        "the most recent with_now call must win for now()"
    );
}
