use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use nitinol_runtime::error::HandlerError;
use nitinol_runtime::ident::ProcessName;
use nitinol_runtime::process::{Driver, Process, ProcessContext};
use nitinol_runtime::{IdleTimeout, ProcessSystem, Props};

struct TickProcess {
    ticks: Arc<AtomicU32>,
    started: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
}

impl Process for TickProcess {
    fn on_start(&mut self, _ctx: &mut ProcessContext<Self>) -> impl Future<Output = ()> + Send {
        self.started.store(true, Ordering::SeqCst);
        async {}
    }
    fn on_stop(&mut self, _ctx: &mut ProcessContext<Self>) -> impl Future<Output = ()> + Send {
        self.stopped.store(true, Ordering::SeqCst);
        async {}
    }
}

fn tick_props(
    ticks: Arc<AtomicU32>,
    started: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
) -> Props<TickProcess> {
    Props::new(move || TickProcess {
        ticks: Arc::clone(&ticks),
        started: Arc::clone(&started),
        stopped: Arc::clone(&stopped),
    })
}

struct ChannelDriver {
    rx: mpsc::Receiver<()>,
    supports_idle: bool,
}

impl Driver<TickProcess> for ChannelDriver {
    type Event = ();

    fn next(&mut self) -> impl Future<Output = Option<Self::Event>> + Send {
        self.rx.recv()
    }

    async fn apply(
        &mut self,
        state: &mut TickProcess,
        _ctx: &mut ProcessContext<TickProcess>,
        _ev: (),
    ) -> Result<(), HandlerError> {
        state.ticks.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn supports_idle_timeout(&self) -> bool {
        self.supports_idle
    }
}

fn fresh_state() -> (Arc<AtomicU32>, Arc<AtomicBool>, Arc<AtomicBool>) {
    (
        Arc::new(AtomicU32::new(0)),
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicBool::new(false)),
    )
}

async fn wait_for_flag(flag: &AtomicBool, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !flag.load(Ordering::SeqCst) {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

async fn wait_for_count(counter: &AtomicU32, expected: u32, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while counter.load(Ordering::SeqCst) < expected {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what} (last value = {})",
            counter.load(Ordering::SeqCst)
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test]
async fn spawn_with_driver_returns_proxies_with_unique_pids() {
    let system = ProcessSystem::new().await;
    let (ticks_a, started_a, stopped_a) = fresh_state();
    let (ticks_b, started_b, stopped_b) = fresh_state();
    let (_tx_a, rx_a) = mpsc::channel::<()>(4);
    let (_tx_b, rx_b) = mpsc::channel::<()>(4);

    let proxy_a = system
        .spawn_with_driver(
            tick_props(ticks_a, started_a, stopped_a),
            ChannelDriver {
                rx: rx_a,
                supports_idle: true,
            },
        )
        .await;
    let proxy_b = system
        .spawn_with_driver(
            tick_props(ticks_b, started_b, stopped_b),
            ChannelDriver {
                rx: rx_b,
                supports_idle: true,
            },
        )
        .await;

    assert_ne!(
        proxy_a.pid(),
        proxy_b.pid(),
        "each driver-backed process must receive its own unique Pid"
    );
}

#[tokio::test]
async fn spawn_with_driver_calls_driver_apply_for_each_delivered_event() {
    let system = ProcessSystem::new().await;
    let (ticks, started, stopped) = fresh_state();
    let (tx, rx) = mpsc::channel::<()>(4);
    let _proxy = system
        .spawn_with_driver(
            tick_props(Arc::clone(&ticks), Arc::clone(&started), stopped),
            ChannelDriver {
                rx,
                supports_idle: true,
            },
        )
        .await;
    wait_for_flag(&started, "on_start").await;

    tx.send(()).await.expect("driver source send");
    tx.send(()).await.expect("driver source send");
    tx.send(()).await.expect("driver source send");

    wait_for_count(&ticks, 3, "ticks==3").await;
}

#[tokio::test]
async fn spawn_with_driver_invokes_on_start_via_lifecycle_loop() {
    let system = ProcessSystem::new().await;
    let (ticks, started, stopped) = fresh_state();
    let (_tx, rx) = mpsc::channel::<()>(4);

    let _proxy = system
        .spawn_with_driver(
            tick_props(ticks, Arc::clone(&started), stopped),
            ChannelDriver {
                rx,
                supports_idle: true,
            },
        )
        .await;

    wait_for_flag(&started, "on_start (driver-backed)").await;
}

#[tokio::test]
async fn spawn_named_with_driver_registers_process_under_alias() {
    let system = ProcessSystem::new().await;
    let (ticks, started, stopped) = fresh_state();
    let (_tx, rx) = mpsc::channel::<()>(4);
    let name = ProcessName::new("named-tick-driver");

    let proxy = system
        .spawn_named_with_driver(
            name.clone(),
            tick_props(ticks, Arc::clone(&started), stopped),
            ChannelDriver {
                rx,
                supports_idle: true,
            },
        )
        .await;
    wait_for_flag(&started, "on_start (named driver-backed)").await;

    let found = system
        .lookup_by_name(&name)
        .await
        .expect("named driver-backed process must be discoverable by name");
    let typed = found
        .downcast::<TickProcess>()
        .expect("registry alias must point at the same downcast-able proxy type");
    assert_eq!(
        typed.pid(),
        proxy.pid(),
        "the alias must resolve to the same Pid spawn_named_with_driver returned"
    );
}

#[tokio::test]
async fn spawn_with_driver_disarms_idle_timer_when_driver_opts_out() {
    let system = ProcessSystem::new().await;
    let (ticks, started, stopped) = fresh_state();
    let (_tx, rx) = mpsc::channel::<()>(4);

    let mut props = tick_props(ticks, Arc::clone(&started), Arc::clone(&stopped));
    props.with_idle_timeout(IdleTimeout::After(Duration::from_millis(30)));
    let proxy = system
        .spawn_with_driver(
            props,
            ChannelDriver {
                rx,
                supports_idle: false,
            },
        )
        .await;
    wait_for_flag(&started, "on_start (idle-disarmed)").await;

    tokio::time::sleep(Duration::from_millis(150)).await;

    assert!(
        !stopped.load(Ordering::SeqCst),
        "spawn_with_driver must disarm the idle-timeout timer when the driver \
         reports supports_idle_timeout() == false, even if Props configured \
         IdleTimeout::After"
    );

    proxy.stop().await.expect("stop must succeed");
    wait_for_flag(&stopped, "on_stop after explicit stop").await;
}

#[tokio::test]
async fn signal_stop_nonblocking_stops_a_live_process_without_awaiting() {
    let system = ProcessSystem::new().await;
    let (ticks, started, stopped) = fresh_state();
    let (_tx, rx) = mpsc::channel::<()>(4);
    let proxy = system
        .spawn_with_driver(
            tick_props(ticks, Arc::clone(&started), Arc::clone(&stopped)),
            ChannelDriver {
                rx,
                supports_idle: false,
            },
        )
        .await;
    wait_for_flag(&started, "on_start (sync stop)").await;

    proxy.signal_stop_nonblocking();

    wait_for_flag(&stopped, "on_stop after signal_stop_nonblocking").await;
}

#[tokio::test]
async fn signal_stop_nonblocking_is_safe_on_an_already_stopped_process() {
    let system = ProcessSystem::new().await;
    let (ticks, started, stopped) = fresh_state();
    let (_tx, rx) = mpsc::channel::<()>(4);
    let proxy = system
        .spawn_with_driver(
            tick_props(ticks, Arc::clone(&started), Arc::clone(&stopped)),
            ChannelDriver {
                rx,
                supports_idle: false,
            },
        )
        .await;
    wait_for_flag(&started, "on_start (idempotent stop)").await;
    proxy.stop().await.expect("initial stop must succeed");
    wait_for_flag(&stopped, "on_stop after initial stop").await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    proxy.signal_stop_nonblocking();
}

struct TellableProcess;

impl Process for TellableProcess {}

impl nitinol_runtime::process::Receive<u32> for TellableProcess {
    type Response = ();
    type Error = std::convert::Infallible;
    async fn recv(
        &mut self,
        _msg: u32,
        _ctx: &mut ProcessContext<Self>,
    ) -> Result<(), std::convert::Infallible> {
        Ok(())
    }
}

struct NeverDriver;

impl Driver<TellableProcess> for NeverDriver {
    type Event = ();
    fn next(&mut self) -> impl Future<Output = Option<()>> + Send {
        std::future::pending()
    }
    async fn apply(
        &mut self,
        _state: &mut TellableProcess,
        _ctx: &mut ProcessContext<TellableProcess>,
        _ev: (),
    ) -> Result<(), HandlerError> {
        Ok(())
    }
    fn supports_idle_timeout(&self) -> bool {
        false
    }
}

#[tokio::test]
async fn spawn_with_driver_tell_returns_error_because_user_rx_is_dropped() {
    let system = ProcessSystem::new().await;
    let proxy = system
        .spawn_with_driver(Props::new(|| TellableProcess), NeverDriver)
        .await;

    let result = proxy.tell(42u32).await;
    assert!(
        result.is_err(),
        "tell must return an error for driver-backed processes (user_rx is dropped)"
    );
}
