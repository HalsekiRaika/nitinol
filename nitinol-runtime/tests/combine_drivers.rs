//! Tests for the `Combine<L, R>` driver and the `combine_drivers!` macro
//! introduced in Issue #53.
//!
//! These tests pin down:
//! - `Combine<L, R>` dispatches each event to the correct side's `apply()`.
//! - The `combine_drivers!` macro is single-arg identity (no wrapping).
//! - The `combine_drivers!` macro right-folds 3+ args into nested `Combine`s.
//! - `supports_idle_timeout()` is AND-combined across children
//!   (any `false` poisons the combined result).
//! - `pipe_handle()` is propagated up: if any child has a handle, Combine
//!   yields `Some(...)`; if neither does, Combine yields `None`.
//! - Without a `PipeDriver` anywhere in the tree, the combined driver has
//!   no pipe handle — explicit composition is required for the
//!   `spawn_with_driver` path.

use std::future::Future;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use nitinol_runtime::combine_drivers;
use nitinol_runtime::error::HandlerError;
use nitinol_runtime::process::{Combine, Driver, PipeDriver, Process, ProcessContext};
use nitinol_runtime::{ProcessSystem, Props};

// ---------------------------------------------------------------------------
// Test process: counts events delivered by each side of a Combine driver.
// ---------------------------------------------------------------------------

struct CounterProcess;

impl Process for CounterProcess {}

fn counter_props() -> Props<CounterProcess> {
    Props::new(|| CounterProcess)
}

// ---------------------------------------------------------------------------
// Three single-purpose drivers. Each `apply` bumps a distinct counter so the
// test can tell which branch of the Combine tree fired.
// ---------------------------------------------------------------------------

struct IntDriver {
    rx: mpsc::Receiver<i32>,
    counter: Arc<AtomicU32>,
}

impl Driver<CounterProcess> for IntDriver {
    type Event = i32;

    fn next(&mut self) -> impl Future<Output = Option<Self::Event>> + Send {
        self.rx.recv()
    }

    async fn apply(
        &mut self,
        _state: &mut CounterProcess,
        _ctx: &mut ProcessContext<CounterProcess>,
        _ev: Self::Event,
    ) -> Result<(), HandlerError> {
        self.counter.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

struct StrDriver {
    rx: mpsc::Receiver<String>,
    counter: Arc<AtomicU32>,
}

impl Driver<CounterProcess> for StrDriver {
    type Event = String;

    fn next(&mut self) -> impl Future<Output = Option<Self::Event>> + Send {
        self.rx.recv()
    }

    async fn apply(
        &mut self,
        _state: &mut CounterProcess,
        _ctx: &mut ProcessContext<CounterProcess>,
        _ev: Self::Event,
    ) -> Result<(), HandlerError> {
        self.counter.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

struct BoolDriver {
    rx: mpsc::Receiver<bool>,
    counter: Arc<AtomicU32>,
}

impl Driver<CounterProcess> for BoolDriver {
    type Event = bool;

    fn next(&mut self) -> impl Future<Output = Option<Self::Event>> + Send {
        self.rx.recv()
    }

    async fn apply(
        &mut self,
        _state: &mut CounterProcess,
        _ctx: &mut ProcessContext<CounterProcess>,
        _ev: Self::Event,
    ) -> Result<(), HandlerError> {
        self.counter.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn wait_for_count(counter: &AtomicU32, expected: u32, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while counter.load(Ordering::SeqCst) < expected {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}; observed {}",
            counter.load(Ordering::SeqCst)
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn fresh_counters() -> (Arc<AtomicU32>, Arc<AtomicU32>, Arc<AtomicU32>) {
    (
        Arc::new(AtomicU32::new(0)),
        Arc::new(AtomicU32::new(0)),
        Arc::new(AtomicU32::new(0)),
    )
}

// ---------------------------------------------------------------------------
// Drivers used for non-spawning unit-style checks (`supports_idle_timeout`,
// `pipe_handle`). They never produce events.
// ---------------------------------------------------------------------------

struct IdleDriverTrue;
struct IdleDriverFalse;

impl Driver<CounterProcess> for IdleDriverTrue {
    type Event = ();
    fn next(&mut self) -> impl Future<Output = Option<Self::Event>> + Send {
        std::future::pending()
    }
    async fn apply(
        &mut self,
        _state: &mut CounterProcess,
        _ctx: &mut ProcessContext<CounterProcess>,
        _ev: (),
    ) -> Result<(), HandlerError> {
        Ok(())
    }
    fn supports_idle_timeout(&self) -> bool {
        true
    }
}

impl Driver<CounterProcess> for IdleDriverFalse {
    type Event = ();
    fn next(&mut self) -> impl Future<Output = Option<Self::Event>> + Send {
        std::future::pending()
    }
    async fn apply(
        &mut self,
        _state: &mut CounterProcess,
        _ctx: &mut ProcessContext<CounterProcess>,
        _ev: (),
    ) -> Result<(), HandlerError> {
        Ok(())
    }
    fn supports_idle_timeout(&self) -> bool {
        false
    }
}

// ---------------------------------------------------------------------------
// Macro arity 1: `combine_drivers!(A)` is an identity, not a wrapped Combine.
// ---------------------------------------------------------------------------

/// Given a single driver value,
/// when wrapped in `combine_drivers!(d)`,
/// then the expression evaluates to the same driver (NOT a `Combine`).
///
/// Type-level check: assigning to a binding of the original driver's type
/// only compiles if the macro is single-arg identity. If the macro accidentally
/// wraps even single args in `Combine`, the assignment fails because
/// `Combine<A, _>` is not `A`.
#[tokio::test]
async fn combine_drivers_macro_single_arg_is_identity() {
    let (_tx, rx) = mpsc::channel::<i32>(4);
    let driver = IntDriver {
        rx,
        counter: Arc::new(AtomicU32::new(0)),
    };

    // Type annotation forces the macro to produce an `IntDriver` and not
    // a `Combine`. If `combine_drivers!(A)` were `Combine::new(A, ())` or
    // similar, this would fail to compile.
    let single: IntDriver = combine_drivers!(driver);

    // Sanity behavior check: the identity-driver still behaves as a Driver.
    assert!(
        single.supports_idle_timeout(),
        "single-arg combine_drivers must preserve the original Driver's \
         supports_idle_timeout"
    );
}

// ---------------------------------------------------------------------------
// Macro arity 2: `combine_drivers!(A, B)` produces a `Combine<A, B>` that
// dispatches each event to the originating side's `apply`.
// ---------------------------------------------------------------------------

/// Given a Combine of two drivers (one for `i32`, one for `String`),
/// when events are sent on both channels,
/// then both sides' `apply` runs and the respective counters increment.
/// This pins down that Combine multiplexes the two sources without dropping
/// events, and that the lifecycle loop only sees ONE driver (Combine), with
/// multiplexing happening inside Combine itself.
#[tokio::test]
async fn combine_drivers_two_args_dispatch_events_to_each_side() {
    let system = ProcessSystem::new().await;
    let (int_cnt, str_cnt, _bool_cnt) = fresh_counters();
    let (int_tx, int_rx) = mpsc::channel::<i32>(8);
    let (str_tx, str_rx) = mpsc::channel::<String>(8);

    let driver = combine_drivers!(
        IntDriver {
            rx: int_rx,
            counter: int_cnt.clone(),
        },
        StrDriver {
            rx: str_rx,
            counter: str_cnt.clone(),
        }
    );

    let _proxy = system
        .spawn_with_driver(
            counter_props(),
            driver,
        )
        .await;

    int_tx.send(1).await.expect("int send");
    int_tx.send(2).await.expect("int send");
    str_tx.send("a".into()).await.expect("str send");

    wait_for_count(&int_cnt, 2, "int counter = 2").await;
    wait_for_count(&str_cnt, 1, "str counter = 1").await;

    assert_eq!(int_cnt.load(Ordering::SeqCst), 2);
    assert_eq!(str_cnt.load(Ordering::SeqCst), 1);
}

// ---------------------------------------------------------------------------
// Macro arity 3: `combine_drivers!(A, B, C)` right-folds to
// `Combine::new(A, Combine::new(B, C))`. All three sources must deliver.
// ---------------------------------------------------------------------------

/// Given a Combine of three drivers built via `combine_drivers!`,
/// when events are sent on all three channels,
/// then all three sides' `apply` run. This is the key macro property:
/// the right-fold doesn't lose any branch.
#[tokio::test]
async fn combine_drivers_three_args_dispatch_events_to_all_three_sides() {
    let system = ProcessSystem::new().await;
    let (int_cnt, str_cnt, bool_cnt) = fresh_counters();
    let (int_tx, int_rx) = mpsc::channel::<i32>(8);
    let (str_tx, str_rx) = mpsc::channel::<String>(8);
    let (bool_tx, bool_rx) = mpsc::channel::<bool>(8);

    let driver = combine_drivers!(
        IntDriver {
            rx: int_rx,
            counter: int_cnt.clone(),
        },
        StrDriver {
            rx: str_rx,
            counter: str_cnt.clone(),
        },
        BoolDriver {
            rx: bool_rx,
            counter: bool_cnt.clone(),
        }
    );

    let _proxy = system
        .spawn_with_driver(
            counter_props(),
            driver,
        )
        .await;

    int_tx.send(7).await.expect("int send");
    str_tx.send("x".into()).await.expect("str send");
    bool_tx.send(true).await.expect("bool send");

    wait_for_count(&int_cnt, 1, "int counter = 1").await;
    wait_for_count(&str_cnt, 1, "str counter = 1").await;
    wait_for_count(&bool_cnt, 1, "bool counter = 1").await;
}

// ---------------------------------------------------------------------------
// `supports_idle_timeout()` is AND-combined across children. The existing
// MessageDriver-default `true` plus a tick-style `false` must collapse to
// `false` so existing IntervalDriver semantics are preserved.
// ---------------------------------------------------------------------------

/// Given a Combine of two `supports_idle_timeout() == true` drivers,
/// when `supports_idle_timeout()` is queried,
/// then the combined value is `true`.
#[tokio::test]
async fn combine_supports_idle_timeout_is_true_when_both_children_are_true() {
    let combined: Combine<IdleDriverTrue, IdleDriverTrue> =
        Combine::new(IdleDriverTrue, IdleDriverTrue);
    assert!(
        combined.supports_idle_timeout(),
        "Combine must AND-combine children: true && true == true"
    );
}

/// Given a Combine where one child has `supports_idle_timeout() == false`,
/// when `supports_idle_timeout()` is queried,
/// then the combined value is `false`. This pins down the preservation rule
/// for IntervalDriver semantics: any tick-style child must poison the whole
/// combined idle behavior.
#[tokio::test]
async fn combine_supports_idle_timeout_is_false_when_left_child_is_false() {
    let combined: Combine<IdleDriverFalse, IdleDriverTrue> =
        Combine::new(IdleDriverFalse, IdleDriverTrue);
    assert!(
        !combined.supports_idle_timeout(),
        "Combine must AND-combine children: false && true == false"
    );
}

/// Given a Combine where the RIGHT child has `supports_idle_timeout() == false`,
/// when `supports_idle_timeout()` is queried,
/// then the combined value is `false`. Symmetry check vs. the left-side test.
#[tokio::test]
async fn combine_supports_idle_timeout_is_false_when_right_child_is_false() {
    let combined: Combine<IdleDriverTrue, IdleDriverFalse> =
        Combine::new(IdleDriverTrue, IdleDriverFalse);
    assert!(
        !combined.supports_idle_timeout(),
        "Combine must AND-combine children: true && false == false"
    );
}

/// Given a Combine where both children have `supports_idle_timeout() == false`,
/// when `supports_idle_timeout()` is queried,
/// then the combined value is `false`.
#[tokio::test]
async fn combine_supports_idle_timeout_is_false_when_both_children_are_false() {
    let combined: Combine<IdleDriverFalse, IdleDriverFalse> =
        Combine::new(IdleDriverFalse, IdleDriverFalse);
    assert!(
        !combined.supports_idle_timeout(),
        "Combine must AND-combine children: false && false == false"
    );
}

// ---------------------------------------------------------------------------
// `pipe_handle()` propagation: Combine surfaces whichever child holds it.
// ---------------------------------------------------------------------------

/// Given a Combine where the LEFT child is `PipeDriver`,
/// when `pipe_handle()` is queried,
/// then `Some(handle)` is returned. The lifecycle loop relies on this
/// propagation to wire `ctx.pipe_to_self` automatically.
#[tokio::test]
async fn combine_pipe_handle_is_some_when_left_child_has_pipe_driver() {
    let combined: Combine<PipeDriver<CounterProcess>, IdleDriverTrue> =
        Combine::new(PipeDriver::new(), IdleDriverTrue);
    assert!(
        combined.pipe_handle().is_some(),
        "Combine must surface left child's pipe_handle when the right child has none"
    );
}

/// Given a Combine where the RIGHT child is `PipeDriver`,
/// when `pipe_handle()` is queried,
/// then `Some(handle)` is returned. Symmetric to the left-side test.
#[tokio::test]
async fn combine_pipe_handle_is_some_when_right_child_has_pipe_driver() {
    let combined: Combine<IdleDriverTrue, PipeDriver<CounterProcess>> =
        Combine::new(IdleDriverTrue, PipeDriver::new());
    assert!(
        combined.pipe_handle().is_some(),
        "Combine must surface right child's pipe_handle when the left child has none"
    );
}

/// Given a Combine of two non-pipe drivers,
/// when `pipe_handle()` is queried,
/// then `None` is returned — there is no PipeDriver anywhere in the tree.
/// This is the precondition that triggers the "PipeDriver not composed"
/// panic at `ctx.pipe_to_self` call sites.
#[tokio::test]
async fn combine_pipe_handle_is_none_when_no_child_has_pipe_driver() {
    let combined: Combine<IdleDriverTrue, IdleDriverFalse> =
        Combine::new(IdleDriverTrue, IdleDriverFalse);
    assert!(
        combined.pipe_handle().is_none(),
        "Combine must return None when no child contributes a pipe handle; \
         this is the signal the runtime uses to decide ctx.pipe_to_self is unconfigured"
    );
}

/// Given a deeply nested Combine where the PipeDriver lives at the leaf
/// of a right-fold tree (`Combine<A, Combine<B, PipeDriver>>`),
/// when `pipe_handle()` is queried at the root,
/// then it is still propagated up through the inner Combine. This is the
/// realistic shape that `combine_drivers!(A, B, PipeDriver::new())` produces.
#[tokio::test]
async fn combine_pipe_handle_propagates_through_nested_combines() {
    let combined: Combine<IdleDriverTrue, Combine<IdleDriverTrue, PipeDriver<CounterProcess>>> =
        Combine::new(
            IdleDriverTrue,
            Combine::new(IdleDriverTrue, PipeDriver::<CounterProcess>::new()),
        );
    assert!(
        combined.pipe_handle().is_some(),
        "Combine::pipe_handle must recurse into nested Combines so that \
         right-folded `combine_drivers!(A, B, PipeDriver::new())` still surfaces \
         the handle at the root"
    );
}

// ---------------------------------------------------------------------------
// Type-level checks. These force the published API surface to remain stable.
// ---------------------------------------------------------------------------

#[allow(dead_code)]
fn _assert_combine_is_driver<P: Process>(combine: Combine<IdleDriverTrue, IdleDriverTrue>)
where
    Combine<IdleDriverTrue, IdleDriverTrue>: Driver<CounterProcess>,
{
    // Static check: `Combine<L, R>` implements `Driver<P>` when both L and R
    // do. If the impl is ever lost, this stops compiling.
    let _ = combine;
    let _ = std::marker::PhantomData::<P>;
}

#[allow(dead_code)]
fn _assert_combine_new_takes_two_args() {
    // Constructor must accept exactly two arguments. If `Combine::new` ever
    // grows additional parameters (or shrinks), this signature breaks.
    let _: Combine<IdleDriverTrue, IdleDriverTrue> =
        Combine::new(IdleDriverTrue, IdleDriverTrue);
}
