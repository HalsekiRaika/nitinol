mod capacity;
mod idle_timeout;
mod parts;
mod strategy;

pub use self::capacity::{MailboxCapacity, PipeCapacity, StashCapacity};
pub use self::idle_timeout::IdleTimeout;
pub use self::strategy::{RestartConfig, SupervisionStrategy};

pub(crate) use self::capacity::{resolve_mailbox, resolve_pipe, resolve_stash};
pub(crate) use self::idle_timeout::resolve_idle_timeout;
pub(crate) use self::parts::PropsParts;

use crate::ident::ProcessName;
use crate::process::driver::{boxed_dyn_driver, Driver, DynDriver};
use crate::process::supervision::SupervisionConfig;
use crate::process::Process;

/// Aggregated declaration of every per-process resource the runtime needs to
/// spawn `P` — mailbox / stash / pipe capacity, custom drivers, supervision,
/// idle timeout, and optional name (Issue #56).
///
/// Construct via [`Props::new`], then chain `with_*` / `add_driver` calls.
/// Each setter consumes `self` and returns `Self` so the whole declaration
/// fits a single expression at the spawn call site.
pub struct Props<P: Process> {
    producer: Box<dyn Fn() -> P + Send + Sync>,
    supervision_strategy: SupervisionStrategy,
    idle_timeout: IdleTimeout,
    mailbox_capacity: MailboxCapacity,
    stash_capacity: StashCapacity,
    pipe_capacity: PipeCapacity,
    custom_drivers: Vec<Box<dyn DynDriver<P>>>,
    name: Option<ProcessName>,
}

impl<P: Process> Props<P> {
    pub fn new(producer: impl Fn() -> P + Send + Sync + 'static) -> Self {
        Self {
            producer: Box::new(producer),
            supervision_strategy: SupervisionStrategy::Stop,
            idle_timeout: IdleTimeout::default(),
            mailbox_capacity: MailboxCapacity::Inherit,
            stash_capacity: StashCapacity::Inherit,
            pipe_capacity: PipeCapacity::Inherit,
            custom_drivers: Vec::new(),
            name: None,
        }
    }

    pub fn with_supervision_strategy(mut self, strategy: SupervisionStrategy) -> Self {
        self.supervision_strategy = strategy;
        self
    }

    pub fn with_idle_timeout(mut self, timeout: IdleTimeout) -> Self {
        self.idle_timeout = timeout;
        self
    }

    pub fn with_mailbox_capacity(mut self, capacity: MailboxCapacity) -> Self {
        self.mailbox_capacity = capacity;
        self
    }

    pub fn with_stash_capacity(mut self, capacity: StashCapacity) -> Self {
        self.stash_capacity = capacity;
        self
    }

    pub fn with_pipe_capacity(mut self, capacity: PipeCapacity) -> Self {
        self.pipe_capacity = capacity;
        self
    }

    pub fn with_name(mut self, name: ProcessName) -> Self {
        self.name = Some(name);
        self
    }

    /// Compose `driver` into the per-process driver tree.
    ///
    /// Custom drivers are layered on top of the Core trio
    /// (`MessageDriver` + `PipeDriver` + `StashDriver`) that
    /// `ProcessSystem::spawn` automatically attaches.
    pub fn add_driver<D: Driver<P>>(mut self, driver: D) -> Self {
        self.custom_drivers.push(boxed_dyn_driver(driver));
        self
    }

    /// Decompose into the underlying named-field parts.
    ///
    /// `pub(crate)` boundary keeps `PropsParts` an implementation detail of
    /// the spawn pipeline; users interact only through the builder methods.
    pub(crate) fn into_parts(self) -> PropsParts<P> {
        let initial = (self.producer)();
        let supervision = SupervisionConfig {
            producer: self.producer,
            strategy: self.supervision_strategy,
        };
        PropsParts {
            initial,
            supervision,
            idle_timeout: self.idle_timeout,
            mailbox_capacity: self.mailbox_capacity,
            stash_capacity: self.stash_capacity,
            pipe_capacity: self.pipe_capacity,
            custom_drivers: self.custom_drivers,
            name: self.name,
        }
    }
}
