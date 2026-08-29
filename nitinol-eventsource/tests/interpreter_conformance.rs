//! The conformance suite, run against the interpreter this crate ships.
//!
//! ```gherkin
//! Scenario: an interpreter is checked against the laws
//!   Given the conformance kit and the aggregate activation this crate ships
//!   When the suite is run as part of this crate's ordinary test run
//!   Then every clause from L-1 to L-9 holds
//! ```
//!
//! The suite is what makes the laws executable for *any* interpreter: it drives
//! the kit's own decider through the activation and reads the resulting stream
//! back itself, so nothing here depends on the activation agreeing with its own
//! account of what it did.
//!
//! Running it needs no feature flag, because a law that only holds under an
//! opt-in configuration is not a law this crate keeps.
//!
//! The subscriber that captures the records a told refusal is surfaced through
//! is installed globally, which can happen once per process, so this file holds
//! exactly one test.

#[path = "common/interpreters.rs"]
mod common;
use common::{ActivationInterpretation, ReportCapture};

use nitinol_conformance::verify;
use nitinol_runtime::ProcessSystem;

/// Every clause of the suite must hold for the aggregate activation this crate
/// interprets a `Decision` with.
///
/// `verify` names the clause it found broken, so a failure here reads as the
/// law that was violated rather than as an assertion in this file.
#[tokio::test]
async fn the_aggregate_activation_conforms_to_every_clause() {
    // Given
    let reports = ReportCapture::install();
    let process_system = ProcessSystem::new().await;
    let activation = ActivationInterpretation::new(process_system, reports);

    // When / Then
    verify(&activation).await;
}
