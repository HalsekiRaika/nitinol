//! Tests for the `Props![P; D1, D2, ..., Dn]` type-position macro.
//!
//! These tests pin down:
//!
//! 1. `Props![P; D]` (arity 1) is **identity** — equals `Props<P, D<P>>`,
//!    NOT wrapped in `Combine`. This is the required short-circuit for the
//!    single-driver case.
//! 2. `Props![P; D1, D2]` (arity 2) equals `Props<P, Combine<D1<P>, D2<P>>>`
//!    — the right-fold base case.
//! 3. `Props![P; D1, D2, D3]` (arity 3+) right-folds to
//!    `Props<P, Combine<D1<P>, Combine<D2<P>, D3<P>>>>`. The macro must
//!    produce the same nested shape that `combine_drivers!` produces at the
//!    value level so the two line up exactly through `with_driver`.
//! 4. `P` may itself be a generic type (e.g. `GenericProc<T, U>`) — the head
//!    capture must accept arbitrary type expressions.
//! 5. A trailing comma after the driver list is accepted.
//! 6. A macro-typed `Props` value is constructible AND spawnable end-to-end:
//!    the resulting process actually receives events through the driver tree
//!    on the runtime, proving the macro expansion is a real, working type
//!    (not just a syntactic equivalent).
//! 7. The macro is re-exported at the crate root (`nitinol_runtime::Props`)
//!    by `#[macro_export]`; the same `use nitinol_runtime::Props;` brings in
//!    both the `Props` type and the `Props!` macro (different namespaces,
//!    same path).
//!
//! Compile-error cases (`Props![P]` / `Props![P;]` should NOT compile) cannot
//! be expressed from a Cargo integration-test file — there is no `trybuild`
//! infrastructure in this workspace. Those negative cases are covered by
//! `compile_fail` doctests on the macro itself in `src/process/props.rs`.

use std::future::Future;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use nitinol_runtime::combine_drivers;
use nitinol_runtime::error::HandlerError;
use nitinol_runtime::process::{Combine, Driver, Process, ProcessContext};
use nitinol_runtime::{ProcessSystem, Props};

// Fixtures: a marker process and a generic driver that takes `P` as its type
// parameter so the macro's `D<P>` substitution actually has something to
// substitute into. This mirrors the real `IntervalDriver<P>` shape used at
// `nitinol-eventsource/src/durable_stream/poller.rs:30`, which is what the
// `Props!` macro is designed to support.

struct CounterProcess;

impl Process for CounterProcess {}

/// Driver generic over the process type — its only job is to forward `()`
/// events from a channel into a shared counter so behavior tests can observe
/// which branch of the `Combine` tree fired.
struct TickDriver<P> {
    rx: mpsc::Receiver<()>,
    counter: Arc<AtomicU32>,
    _marker: PhantomData<fn() -> P>,
}

impl<P: Process> Driver<P> for TickDriver<P> {
    type Event = ();

    fn next(&mut self) -> impl Future<Output = Option<Self::Event>> + Send {
        self.rx.recv()
    }

    async fn apply(
        &mut self,
        _state: &mut P,
        _ctx: &mut ProcessContext<P>,
        _ev: (),
    ) -> Result<(), HandlerError> {
        self.counter.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    // Tests are about the macro expansion, not idle-timeout interaction —
    // returning false here keeps the idle timer disarmed so it can never
    // accidentally stop a test process mid-assertion.
    fn supports_idle_timeout(&self) -> bool {
        false
    }
}

/// A second generic-over-`P` driver type used to distinguish branches in the
/// `Combine` tree (so type-pin tests can tell `Combine<A<P>, B<P>>` apart
/// from `Combine<A<P>, A<P>>` when needed). Same behavior as `TickDriver`.
struct OtherDriver<P> {
    rx: mpsc::Receiver<()>,
    counter: Arc<AtomicU32>,
    _marker: PhantomData<fn() -> P>,
}

impl<P: Process> Driver<P> for OtherDriver<P> {
    type Event = ();

    fn next(&mut self) -> impl Future<Output = Option<Self::Event>> + Send {
        self.rx.recv()
    }

    async fn apply(
        &mut self,
        _state: &mut P,
        _ctx: &mut ProcessContext<P>,
        _ev: (),
    ) -> Result<(), HandlerError> {
        self.counter.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn supports_idle_timeout(&self) -> bool {
        false
    }
}

/// Generic-over-`<T, U>` process used to prove the macro's `P` head capture
/// accepts a type expression with its own generic parameters, not just bare
/// idents.
struct GenericProc<T, U> {
    _t: PhantomData<fn() -> T>,
    _u: PhantomData<fn() -> U>,
}

impl<T, U> Process for GenericProc<T, U>
where
    T: 'static + Send + Sync,
    U: 'static + Send + Sync,
{
}

// Hand-written spellings of the nested `Combine` trees the `Props!` macro is
// expected to produce. Written out by hand — never via the macro — so the pins
// below compare the macro's expansion against an independent definition.

/// `Props<CounterProcess, Combine<D1, Combine<D2, D3>>>` — the arity-3
/// right-fold.
type ThreeDriverRightFoldProps = Props<
    CounterProcess,
    Combine<
        TickDriver<CounterProcess>,
        Combine<OtherDriver<CounterProcess>, TickDriver<CounterProcess>>,
    >,
>;

/// `Props<CounterProcess, Combine<D1, Combine<D2, Combine<D3, D4>>>>` — the
/// arity-4 right-fold, one nesting level deeper than the arity-3 case.
type FourDriverRightFoldProps = Props<
    CounterProcess,
    Combine<
        TickDriver<CounterProcess>,
        Combine<
            OtherDriver<CounterProcess>,
            Combine<TickDriver<CounterProcess>, OtherDriver<CounterProcess>>,
        >,
    >,
>;

/// `Props<GenericProc<i32, String>, Combine<D1, D2>>` — the arity-2 right-fold
/// with a generic type expression, rather than a bare ident, in head position.
type GenericHeadTwoDriverProps = Props<
    GenericProc<i32, String>,
    Combine<TickDriver<GenericProc<i32, String>>, OtherDriver<GenericProc<i32, String>>>,
>;

// Helpers

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

// Type-level pins.
//
// Strategy: declare a function whose parameter is the EXPLICIT type
// `Props<P, ...>` form and whose return type is the macro-form `Props![...]`.
// The function body returns the parameter unchanged, so the only way this
// compiles is if the two forms produce LITERALLY the same type. If the
// macro's expansion ever drifts from the required right-fold shape, these
// signatures stop compiling.
//
// Each fn is `#[allow(dead_code)]` and never called at runtime — the cargo
// test runner only needs them to compile.

/// Pin: `Props![P; D]` with a single driver is the identity case —
/// `Props<P, D<P>>` with NO `Combine` wrapping. If the macro accidentally
/// wraps even the single-driver case in `Combine` (e.g. `Combine<D<P>, ()>`),
/// the function signature stops compiling.
#[allow(dead_code)]
fn _props_macro_one_driver_equals_explicit_no_combine(
    explicit: Props<CounterProcess, TickDriver<CounterProcess>>,
) -> Props![CounterProcess; TickDriver] {
    explicit
}

/// Pin (reverse direction): the macro form is interchangeable with the
/// explicit form in both directions. Combined with the forward-direction pin
/// above, this proves the two types are EQUAL (not just convertible).
#[allow(dead_code)]
fn _props_macro_one_driver_explicit_form_accepts_macro_value(
    from_macro: Props![CounterProcess; TickDriver],
) -> Props<CounterProcess, TickDriver<CounterProcess>> {
    from_macro
}

/// Pin: `Props![P; D1, D2]` (arity 2) equals
/// `Props<P, Combine<D1<P>, D2<P>>>`. This is the right-fold BASE case.
#[allow(dead_code)]
fn _props_macro_two_drivers_equals_explicit_combine(
    explicit: Props<
        CounterProcess,
        Combine<TickDriver<CounterProcess>, OtherDriver<CounterProcess>>,
    >,
) -> Props![CounterProcess; TickDriver, OtherDriver] {
    explicit
}

/// Pin: `Props![P; D1, D2, D3]` right-folds into
/// `Props<P, Combine<D1<P>, Combine<D2<P>, D3<P>>>>` — the RIGHT-FOLD
/// shape. If the macro ever switches to left-fold
/// (`Combine<Combine<D1<P>, D2<P>>, D3<P>>`) this signature stops compiling,
/// because the two `Combine` shapes are distinct types even though both
/// implement `Driver<P>`.
#[allow(dead_code)]
fn _props_macro_three_drivers_right_folds(
    explicit: ThreeDriverRightFoldProps,
) -> Props![CounterProcess; TickDriver, OtherDriver, TickDriver] {
    explicit
}

/// Pin: `Props![P; D1, D2, D3, D4]` continues the right-fold one level deeper
/// (`Combine<D1<P>, Combine<D2<P>, Combine<D3<P>, D4<P>>>>`). Pinned
/// independently of the 3-arg case so a regression in the recursion arm
/// can be localized to the depth at which it appears.
#[allow(dead_code)]
fn _props_macro_four_drivers_right_folds_one_level_deeper(
    explicit: FourDriverRightFoldProps,
) -> Props![CounterProcess; TickDriver, OtherDriver, TickDriver, OtherDriver] {
    explicit
}

/// Pin: trailing comma after the last driver compiles.
///
/// The macro must accept `Props![P; D1, D2,]` (trailing `,`) and produce the
/// same type as `Props![P; D1, D2]`.
#[allow(dead_code)]
fn _props_macro_trailing_comma_accepted(
    explicit: Props<
        CounterProcess,
        Combine<TickDriver<CounterProcess>, OtherDriver<CounterProcess>>,
    >,
) -> Props![CounterProcess; TickDriver, OtherDriver,] {
    explicit
}

/// Pin: trailing comma is accepted on the single-driver case too.
#[allow(dead_code)]
fn _props_macro_trailing_comma_accepted_for_single_driver(
    explicit: Props<CounterProcess, TickDriver<CounterProcess>>,
) -> Props![CounterProcess; TickDriver,] {
    explicit
}

/// Pin: the head `P` may be a generic type — `GenericProc<i32, String>` —
/// not just a bare ident. The macro's `:ty` capture must accept arbitrary
/// type expressions in head position, including those with internal
/// `<...>` brackets and commas, because the `;` delimiter is what
/// terminates the head.
#[allow(dead_code)]
fn _props_macro_generic_head_p_compiles(
    explicit: Props<GenericProc<i32, String>, TickDriver<GenericProc<i32, String>>>,
) -> Props![GenericProc<i32, String>; TickDriver] {
    explicit
}

/// Pin: generic head `P` combined with multi-driver right-fold. Both
/// substitution dimensions must work together — the head with its own
/// `<T, U>` substituted, AND every driver receiving `GenericProc<T, U>`
/// as the `D<P>` argument.
#[allow(dead_code)]
fn _props_macro_generic_head_p_with_multi_drivers_right_folds(
    explicit: GenericHeadTwoDriverProps,
) -> Props![GenericProc<i32, String>; TickDriver, OtherDriver] {
    explicit
}

// Behavior tests.
//
// Type-equality alone does not guarantee the macro produces a USABLE type
// (a bad expansion could still type-check but fail to spawn). These tests
// build a `Props` value whose annotated type is the macro form, hand it to
// `ProcessSystem::spawn`, push events through the driver tree, and assert
// the right counters increment.

/// Given a `Props` typed via `Props![CounterProcess; TickDriver]` (the
/// identity / single-driver case),
/// when the process is spawned and 3 events are pushed onto the driver's
/// channel,
/// then the driver's `apply` runs 3 times — proving the macro expansion
/// is a real `Props<CounterProcess, TickDriver<CounterProcess>>` and the
/// runtime composes the Core trio on top of it as usual.
#[tokio::test]
async fn props_macro_single_driver_spawns_and_dispatches_events() {
    let system = ProcessSystem::new().await;
    let counter = Arc::new(AtomicU32::new(0));
    let (tx, rx) = mpsc::channel::<()>(4);

    // The return type uses the macro. The body uses the explicit
    // construction path. The function only compiles if those agree.
    fn make_props(
        rx: mpsc::Receiver<()>,
        counter: Arc<AtomicU32>,
    ) -> Props![CounterProcess; TickDriver] {
        Props::new(|| CounterProcess).with_driver(TickDriver {
            rx,
            counter,
            _marker: PhantomData,
        })
    }

    let _proxy = system.spawn(make_props(rx, counter.clone())).await;

    tx.send(()).await.expect("driver send 1");
    tx.send(()).await.expect("driver send 2");
    tx.send(()).await.expect("driver send 3");

    wait_for_count(&counter, 3, "single-driver macro counter == 3").await;
}

/// Given a `Props` typed via `Props![CounterProcess; TickDriver, OtherDriver]`
/// (the 2-arg `Combine` base case),
/// when one event is sent on each child channel,
/// then both children's `apply` run — proving the macro's expansion lines
/// up exactly with the value-level `combine_drivers!(a, b)` shape, so the
/// `with_driver` call type-checks AND the runtime correctly multiplexes
/// both sources.
#[tokio::test]
async fn props_macro_two_drivers_spawns_and_dispatches_to_both_branches() {
    let system = ProcessSystem::new().await;
    let cnt_a = Arc::new(AtomicU32::new(0));
    let cnt_b = Arc::new(AtomicU32::new(0));
    let (tx_a, rx_a) = mpsc::channel::<()>(4);
    let (tx_b, rx_b) = mpsc::channel::<()>(4);

    fn make_props(
        rx_a: mpsc::Receiver<()>,
        cnt_a: Arc<AtomicU32>,
        rx_b: mpsc::Receiver<()>,
        cnt_b: Arc<AtomicU32>,
    ) -> Props![CounterProcess; TickDriver, OtherDriver] {
        let combined = combine_drivers!(
            TickDriver {
                rx: rx_a,
                counter: cnt_a,
                _marker: PhantomData,
            },
            OtherDriver {
                rx: rx_b,
                counter: cnt_b,
                _marker: PhantomData,
            },
        );
        Props::new(|| CounterProcess).with_driver(combined)
    }

    let _proxy = system
        .spawn(make_props(rx_a, cnt_a.clone(), rx_b, cnt_b.clone()))
        .await;

    tx_a.send(()).await.expect("tick send");
    tx_b.send(()).await.expect("other send");

    wait_for_count(&cnt_a, 1, "tick counter == 1").await;
    wait_for_count(&cnt_b, 1, "other counter == 1").await;
}

/// Given a `Props` typed via
/// `Props![CounterProcess; TickDriver, OtherDriver, TickDriver]`
/// (3-arg right-fold),
/// when events are sent on all three child channels,
/// then all three sides' `apply` run — proving the right-fold from
/// `combine_drivers!` (value) and `Props!` (type) match at depth ≥ 2, so
/// the `with_driver` call still type-checks and no branch is dropped.
#[tokio::test]
async fn props_macro_three_drivers_spawns_and_dispatches_to_all_branches() {
    let system = ProcessSystem::new().await;
    let cnt_a = Arc::new(AtomicU32::new(0));
    let cnt_b = Arc::new(AtomicU32::new(0));
    let cnt_c = Arc::new(AtomicU32::new(0));
    let (tx_a, rx_a) = mpsc::channel::<()>(4);
    let (tx_b, rx_b) = mpsc::channel::<()>(4);
    let (tx_c, rx_c) = mpsc::channel::<()>(4);

    fn make_props(
        rx_a: mpsc::Receiver<()>,
        cnt_a: Arc<AtomicU32>,
        rx_b: mpsc::Receiver<()>,
        cnt_b: Arc<AtomicU32>,
        rx_c: mpsc::Receiver<()>,
        cnt_c: Arc<AtomicU32>,
    ) -> Props![CounterProcess; TickDriver, OtherDriver, TickDriver] {
        let combined = combine_drivers!(
            TickDriver {
                rx: rx_a,
                counter: cnt_a,
                _marker: PhantomData,
            },
            OtherDriver {
                rx: rx_b,
                counter: cnt_b,
                _marker: PhantomData,
            },
            TickDriver {
                rx: rx_c,
                counter: cnt_c,
                _marker: PhantomData,
            },
        );
        Props::new(|| CounterProcess).with_driver(combined)
    }

    let _proxy = system
        .spawn(make_props(
            rx_a,
            cnt_a.clone(),
            rx_b,
            cnt_b.clone(),
            rx_c,
            cnt_c.clone(),
        ))
        .await;

    tx_a.send(()).await.expect("a send");
    tx_b.send(()).await.expect("b send");
    tx_c.send(()).await.expect("c send");

    wait_for_count(&cnt_a, 1, "branch a counter == 1").await;
    wait_for_count(&cnt_b, 1, "branch b counter == 1").await;
    wait_for_count(&cnt_c, 1, "branch c counter == 1").await;
}

// Re-export sanity: the macro is callable through the same `nitinol_runtime`
// path that exports the `Props` type. If this stops compiling, the macro
// was either not `#[macro_export]`ed at the crate root or its internal
// `$crate::process::Props` / `$crate::process::Combine` paths drifted.

/// Pin: `use nitinol_runtime::Props;` (already at the top of this file)
/// brings the **macro** into scope (different namespace, same identifier as
/// the type). If this stops compiling, the macro's public import contract
/// has regressed.
#[allow(dead_code)]
fn _props_macro_is_reachable_via_crate_root_import() -> Props![CounterProcess; TickDriver] {
    Props::new(|| CounterProcess).with_driver(TickDriver {
        rx: mpsc::channel::<()>(1).1,
        counter: Arc::new(AtomicU32::new(0)),
        _marker: PhantomData,
    })
}
