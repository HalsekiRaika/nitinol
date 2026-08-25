//! DeadlineScheduler subsystem: the resident timer core, its
//! per-saga `Timers` view, the identity types, and the persisted
//! `ScheduleEvent`.

mod event;
mod process;
mod timers;
mod token;

pub(crate) use self::event::{append_schedule_marker, is_schedule_event_type, ScheduleEvent};
pub(crate) use self::process::DispatchFn;
pub(crate) use self::timers::Timers;

pub use self::process::{spawn_scheduler, SchedulerProxy};
pub use self::token::{ScheduleToken, TimerName};
