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
use crate::process::driver::{DefaultDriver, Driver};
use crate::process::supervision::SupervisionConfig;
use crate::process::Process;

/// Aggregated declaration of every per-process resource the runtime needs to
/// spawn `P` — mailbox / stash / pipe capacity, the per-process driver,
/// supervision, idle timeout, and optional name (Issue #56).
///
/// The `D` type parameter (defaulting to [`DefaultDriver`]) holds the single
/// driver composed on top of the Core trio at spawn time. To install a custom
/// driver, call [`Props::with_driver`]; the call transitions the type from
/// `Props<P, DefaultDriver>` to `Props<P, D2>`. Multi-driver composition is
/// done statically via the [`combine_drivers!`](crate::combine_drivers)
/// macro before passing the result to `with_driver` — there is no runtime
/// driver list.
///
/// Construct via [`Props::new`], then chain `with_*` calls. Each setter
/// consumes `self` and returns `Self` (or, for `with_driver`, `Props<P, D2>`)
/// so the whole declaration fits a single expression at the spawn call site.
pub struct Props<P: Process, D = DefaultDriver> {
    producer: Box<dyn Fn() -> P + Send + Sync>,
    supervision_strategy: SupervisionStrategy,
    idle_timeout: IdleTimeout,
    mailbox_capacity: MailboxCapacity,
    stash_capacity: StashCapacity,
    pipe_capacity: PipeCapacity,
    driver: D,
    name: Option<ProcessName>,
}

impl<P: Process> Props<P, DefaultDriver> {
    pub fn new(producer: impl Fn() -> P + Send + Sync + 'static) -> Self {
        Self {
            producer: Box::new(producer),
            supervision_strategy: SupervisionStrategy::Stop,
            idle_timeout: IdleTimeout::default(),
            mailbox_capacity: MailboxCapacity::Inherit,
            stash_capacity: StashCapacity::Inherit,
            pipe_capacity: PipeCapacity::Inherit,
            driver: DefaultDriver,
            name: None,
        }
    }
}

impl<P: Process, D> Props<P, D> {
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

    /// Replace the per-process driver slot with `driver`, returning a
    /// `Props<P, D2>` whose `D` is the newly supplied driver's type.
    ///
    /// The slot is **single**. Calling `with_driver` twice keeps only the
    /// second driver's type — the first one is dropped. To run multiple
    /// drivers in the same process, build a single composite with
    /// [`combine_drivers!`](crate::combine_drivers) and pass it to a single
    /// `with_driver` call. The runtime then composes the user driver on top
    /// of the Core trio (`MessageDriver` + `PipeDriver` + `StashDriver`)
    /// via [`Combine`](crate::process::Combine).
    pub fn with_driver<D2>(self, driver: D2) -> Props<P, D2>
    where
        D2: Driver<P>,
    {
        Props {
            producer: self.producer,
            supervision_strategy: self.supervision_strategy,
            idle_timeout: self.idle_timeout,
            mailbox_capacity: self.mailbox_capacity,
            stash_capacity: self.stash_capacity,
            pipe_capacity: self.pipe_capacity,
            driver,
            name: self.name,
        }
    }
}

impl<P: Process, D: Driver<P>> Props<P, D> {
    /// Decompose into the underlying named-field parts.
    ///
    /// `pub(crate)` boundary keeps `PropsParts` an implementation detail of
    /// the spawn pipeline; users interact only through the builder methods.
    pub(crate) fn into_parts(self) -> PropsParts<P, D> {
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
            driver: self.driver,
            name: self.name,
        }
    }
}
