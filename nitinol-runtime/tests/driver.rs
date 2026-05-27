use std::future::Future;

use tokio::sync::mpsc;

use nitinol_runtime::error::HandlerError;
use nitinol_runtime::process::{Driver, Process, ProcessContext};

struct DummyProcess;

impl Process for DummyProcess {}

struct IntDriver {
    rx: mpsc::Receiver<i32>,
}

impl Driver<DummyProcess> for IntDriver {
    type Event = i32;

    fn next(&mut self) -> impl Future<Output = Option<Self::Event>> + Send {
        self.rx.recv()
    }

    async fn apply(
        &mut self,
        _state: &mut DummyProcess,
        _ctx: &mut ProcessContext,
        _ev: Self::Event,
    ) -> Result<(), HandlerError> {
        Ok(())
    }
}

struct AlwaysFailingDriver;

impl Driver<DummyProcess> for AlwaysFailingDriver {
    type Event = ();

    async fn next(&mut self) -> Option<Self::Event> {
        Some(())
    }

    async fn apply(
        &mut self,
        _state: &mut DummyProcess,
        _ctx: &mut ProcessContext,
        _ev: Self::Event,
    ) -> Result<(), HandlerError> {
        Err(HandlerError)
    }
}

struct NeverIdleDriver;

impl Driver<DummyProcess> for NeverIdleDriver {
    type Event = ();

    async fn next(&mut self) -> Option<Self::Event> {
        None
    }

    async fn apply(
        &mut self,
        _state: &mut DummyProcess,
        _ctx: &mut ProcessContext,
        _ev: Self::Event,
    ) -> Result<(), HandlerError> {
        Ok(())
    }

    fn supports_idle_timeout(&self) -> bool {
        false
    }
}

struct DefaultDriver;

impl Driver<DummyProcess> for DefaultDriver {
    type Event = ();

    async fn next(&mut self) -> Option<Self::Event>{
        None
    }

    async fn apply(
        &mut self,
        _state: &mut DummyProcess,
        _ctx: &mut ProcessContext,
        _ev: Self::Event,
    ) -> Result<(), HandlerError> {
        Ok(())
    }
}

#[test]
fn driver_trait_is_visible_from_sibling_crate() {
    let (_tx, rx) = mpsc::channel::<i32>(4);
    let _driver = IntDriver { rx };
}

#[test]
fn handler_error_is_visible_from_sibling_crate() {
    let _err: HandlerError = HandlerError;
}

#[test]
fn external_driver_can_return_handler_error_from_apply() {
    let _driver = AlwaysFailingDriver;
}

fn assert_send<T: Send>() {}
fn assert_static<T: 'static>() {}

#[test]
fn driver_impl_satisfies_send_and_static_bounds() {
    assert_send::<IntDriver>();
    assert_static::<IntDriver>();
    assert_send::<NeverIdleDriver>();
    assert_static::<NeverIdleDriver>();
}

#[test]
fn driver_event_associated_type_is_send() {
    assert_send::<<IntDriver as Driver<DummyProcess>>::Event>();
    assert_send::<<NeverIdleDriver as Driver<DummyProcess>>::Event>();
}

#[test]
fn supports_idle_timeout_defaults_to_true() {
    let driver = DefaultDriver;
    assert!(
        driver.supports_idle_timeout(),
        "Driver::supports_idle_timeout default must be true so MessageDriver \
         preserves existing idle-timeout semantics without an override."
    );
}

#[test]
fn supports_idle_timeout_can_be_overridden_to_false() {
    let driver = NeverIdleDriver;
    assert!(
        !driver.supports_idle_timeout(),
        "Driver::supports_idle_timeout override to false must be honoured."
    );
}

#[tokio::test]
async fn next_yields_some_when_source_produces_event() {
    let (tx, rx) = mpsc::channel::<i32>(4);
    let mut driver = IntDriver { rx };
    tx.send(42).await.expect("send");

    let ev = driver.next().await;

    assert_eq!(ev, Some(42));
}

#[tokio::test]
async fn next_yields_none_when_source_closed() {
    let (tx, rx) = mpsc::channel::<i32>(4);
    drop(tx);
    let mut driver = IntDriver { rx };

    let ev = driver.next().await;

    assert_eq!(ev, None);
}

#[tokio::test]
async fn next_does_not_borrow_process_state() {
    let (tx, rx) = mpsc::channel::<i32>(4);
    let mut driver = IntDriver { rx };
    let mut state = DummyProcess;
    tx.send(1).await.expect("send");

    let state_ref: &mut DummyProcess = &mut state;
    let ev = driver.next().await;
    let _: &mut DummyProcess = state_ref;

    assert_eq!(ev, Some(1));
}

#[tokio::test]
async fn next_delivers_events_in_fifo_order() {
    let (tx, rx) = mpsc::channel::<i32>(4);
    let mut driver = IntDriver { rx };
    tx.send(1).await.expect("send");
    tx.send(2).await.expect("send");
    tx.send(3).await.expect("send");

    let first = driver.next().await;
    let second = driver.next().await;
    let third = driver.next().await;

    assert_eq!(first, Some(1));
    assert_eq!(second, Some(2));
    assert_eq!(third, Some(3));
}
