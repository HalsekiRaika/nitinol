//! DeadlineScheduler subsystem: the resident timer core, its
//! per-saga `Timers` view, the identity types, and the persisted
//! `ScheduleEvent`.

mod event;
mod process;
mod timers;
mod token;

use nitinol_runtime::ProcessSystem;

use crate::saga::Saga;
use crate::subscribe::{DeclaresDeadline, SagaSubscribe};

pub(crate) use self::event::{append_schedule_marker, ScheduleEvent, SCHEDULED_ARM};
pub(crate) use self::process::DispatchFn;
pub(crate) use self::process::SchedulerProxy;
pub(crate) use self::timers::Timers;

pub use self::process::SchedulerProcess;
pub use self::token::{ScheduleToken, TimerName};

/// Resolve the system's one timer core for `S`, if `S` has any use for it.
///
/// The condition is the saga's own declaration: an alphabet with a
/// `#[subscribe(deadline)]` variant is a saga that can receive a firing, and one
/// without has nothing a timer could deliver to.  Reading it from the type means
/// the answer is settled before the saga is built, so the resident is resolved
/// exactly once per spawn and never as a side effect of the first deadline being
/// set.
///
/// `HAS_DEADLINE` is read from `S::Subscribe::Deadline` — the same associated
/// type [`SagaComponents::deadline`](crate::SagaComponents::deadline) requires
/// to equal `HasDeadline<M>` before it hands out a `Deadline<M>` — so a saga
/// that can obtain one always has its resident timer core resolved here; there
/// is no separately-settable fact the two could disagree about.
///
/// [`ProcessSystem::spawn_resident`] keys on the process type, so however many
/// sagas ask, one scheduler exists and they all name it.
///
/// This is the only place a [`SchedulerProxy`] is obtained.  It is not part of
/// the public surface: a user holding a live proxy could arm a timer without
/// writing the fact behind it, and a timer no stream knows about does not
/// survive a restart — nor would the restart know it was lost.
pub(crate) async fn resolve_for<S: Saga>(system: &ProcessSystem) -> Option<SchedulerProxy> {
    if <<S::Subscribe as SagaSubscribe>::Deadline as DeclaresDeadline<S::Subscribe>>::HAS_DEADLINE {
        Some(system.spawn_resident::<SchedulerProcess>().await.into())
    } else {
        None
    }
}
