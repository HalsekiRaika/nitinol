//! Tests for Issue #56: `ProcessContext::stash` / `unstash_all` stub API.
//!
//! Per the plan, the B issue owns the stash machinery implementation
//! (queue / replay logic). Issue #56 only locks in the API surface so that
//! callers can compile against it now and so the B issue lands as a pure
//! behavior change without API churn.
//!
//! Plan §"`ctx.stash` / `ctx.unstash_all` stub" specifies:
//! > test 側では「コンパイルできる」「呼び出してもプロセスが死なない」
//! > ことのみ確認する no-op stub にする。
//!
//! These tests pin down:
//! - `ctx.stash(msg)` is callable from a `Receive` handler with the message
//!   type for which the process implements `Receive<M>`.
//! - `ctx.unstash_all()` is callable from a `Receive` handler.
//! - Calling either does NOT panic and does NOT terminate the process —
//!   the actor continues to handle subsequent messages.
//!
//! NOT covered (deferred to the B issue):
//! - Whether `stash`ed messages are re-delivered.
//! - Stash capacity-overflow behavior.

use std::future::Future;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use nitinol_runtime::process::{Process, ProcessContext, Receive};
use nitinol_runtime::{ProcessSystem, Props};

// ---------------------------------------------------------------------------
// Fixture: a process that stashes/unstashes during its handler and counts how
// many `Ping` messages it has received (proof-of-life).
// ---------------------------------------------------------------------------

#[derive(Default)]
struct StashingProcess {
    pings: Arc<AtomicU32>,
}

struct StashOne;
struct UnstashAll;
struct Ping;

impl Process for StashingProcess {}

impl Receive<StashOne> for StashingProcess {
    type Response = ();
    type Error = std::convert::Infallible;
    async fn recv(
        &mut self,
        msg: StashOne,
        ctx: &mut ProcessContext<Self>,
    ) -> Result<(), std::convert::Infallible> {
        // Pin down the stub-call signature: `ctx.stash(msg)` where `msg` is
        // exactly the message the process implements `Receive<...>` for.
        ctx.stash(msg).await;
        Ok(())
    }
}

impl Receive<UnstashAll> for StashingProcess {
    type Response = ();
    type Error = std::convert::Infallible;
    async fn recv(
        &mut self,
        _: UnstashAll,
        ctx: &mut ProcessContext<Self>,
    ) -> Result<(), std::convert::Infallible> {
        ctx.unstash_all().await;
        Ok(())
    }
}

impl Receive<Ping> for StashingProcess {
    type Response = ();
    type Error = std::convert::Infallible;
    async fn recv(
        &mut self,
        _: Ping,
        _ctx: &mut ProcessContext<Self>,
    ) -> Result<(), std::convert::Infallible> {
        self.pings.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

fn stashing_props(pings: Arc<AtomicU32>) -> Props<StashingProcess> {
    Props::new(move || StashingProcess {
        pings: pings.clone(),
    })
}

async fn wait_for_count(counter: &AtomicU32, expected: u32) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while counter.load(Ordering::SeqCst) < expected {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for counter to reach {expected}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

// ---------------------------------------------------------------------------
// API existence + survival under no-op stub
// ---------------------------------------------------------------------------

/// Given a process whose handler calls `ctx.stash(StashOne)`,
/// when the message is delivered,
/// then the handler returns normally and a subsequent `Ping` is processed —
/// proving the stub is a no-op (does not panic, does not drop the actor).
#[tokio::test]
async fn ctx_stash_is_no_op_stub_and_actor_survives() {
    let system = ProcessSystem::new().await;
    let pings = Arc::new(AtomicU32::new(0));
    let proxy = system.spawn(stashing_props(pings.clone())).await;

    proxy
        .tell(StashOne)
        .await
        .expect("tell StashOne must succeed");

    // Subsequent message MUST be processed — the actor cannot have died.
    proxy.tell(Ping).await.expect("tell Ping after stash must succeed");
    wait_for_count(&pings, 1).await;
}

/// Given a process whose handler calls `ctx.unstash_all()`,
/// when the message is delivered,
/// then the handler returns normally and a subsequent `Ping` is processed —
/// the stub-side `unstash_all` is also a no-op that the actor survives.
#[tokio::test]
async fn ctx_unstash_all_is_no_op_stub_and_actor_survives() {
    let system = ProcessSystem::new().await;
    let pings = Arc::new(AtomicU32::new(0));
    let proxy = system.spawn(stashing_props(pings.clone())).await;

    proxy
        .tell(UnstashAll)
        .await
        .expect("tell UnstashAll must succeed");

    proxy
        .tell(Ping)
        .await
        .expect("tell Ping after unstash_all must succeed");
    wait_for_count(&pings, 1).await;
}

// ---------------------------------------------------------------------------
// Type-level: the methods exist on `ProcessContext<P>` with the spec shape
// (`stash(msg)` requires `P: Receive<M>`; `unstash_all` takes no args).
// ---------------------------------------------------------------------------

/// Compile-only assertion: `ctx.stash(msg).await` and `ctx.unstash_all().await`
/// must both be valid inside a `Receive` handler. If the signatures drift
/// (e.g. `stash` becomes synchronous or `unstash_all` requires arguments),
/// the surrounding test handlers stop compiling and this sentinel keeps the
/// API-shape regression visible.
#[allow(dead_code)]
async fn _compile_assert_stash_api_shape(ctx: &mut ProcessContext<StashingProcess>) {
    ctx.stash(StashOne).await;
    ctx.unstash_all().await;
}

// Suppress unused-import warnings if Future ever drifts out of use.
#[allow(dead_code)]
fn _future_in_scope() -> impl Future<Output = ()> {
    async {}
}
