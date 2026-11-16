// `RetryPolicy`: how many attempts a retry makes, and how long it waits before
// each one after the first.
//
// Observed through what the policy answers, which is everything a retry reads
// from it.

use std::time::Duration;

use nitinol_eventsource::RetryPolicy;

/// The default makes 3 attempts and waits 100 ms before the second and 200 ms
/// before the third.
#[test]
fn the_default_policy_makes_three_attempts_backing_off_from_100ms_doubling() {
    // Given
    let policy = RetryPolicy::default();

    // Then
    assert_eq!(policy.max_attempts(), 3, "the default must make 3 attempts");
    assert_eq!(
        (policy.backoff_before(2), policy.backoff_before(3)),
        (Duration::from_millis(100), Duration::from_millis(200)),
        "the default must wait 100 ms before the second attempt and 200 ms before the third"
    );
}

/// The values a policy is built from are the ones it answers with: the attempt
/// bound as given, and a wait that starts at the initial backoff before the
/// second attempt and grows by the multiplier before each one after.
#[test]
fn a_built_policy_answers_with_the_values_it_was_given() {
    // Given
    let policy = RetryPolicy::new(4, Duration::from_millis(10), 3.0);

    // Then
    assert_eq!(
        policy.max_attempts(),
        4,
        "the attempt bound must be the one given"
    );
    assert_eq!(
        (
            policy.backoff_before(2),
            policy.backoff_before(3),
            policy.backoff_before(4)
        ),
        (
            Duration::from_millis(10),
            Duration::from_millis(30),
            Duration::from_millis(90)
        ),
        "the waits must start at the initial backoff and grow by the multiplier"
    );
}
