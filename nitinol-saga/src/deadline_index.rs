//! The deadline index: what brings an instance back for a deadline when
//! nothing upstream will.
//!
//! A deadline durably lives on the saga's own stream, as the schedule fact the
//! instance wrote when it set it, and the resident scheduler holding its timer
//! is a cache of that fact. The timer is armed again when the instance is next
//! activated and replays its stream — and an instance is activated by its
//! manager, for an upstream record correlated to it or a firing addressed to
//! it. After a restart neither may ever come: the scheduler restores nothing,
//! and the upstream may have nothing new for that instance.
//!
//! So each saga type that declares a deadline is also hosted with this index.
//! It reads every saga's schedule and lifecycle facts in global order, holds
//! the deadlines they leave pending, and when one falls due asks the manager
//! for that instance. The instance comes back the way any other activation
//! brings it back — replay, re-arm, and the scheduler's firing reaching
//! `handle` — so a deadline still has one road to `handle`.
//!
//! # What it does not do
//!
//! It reads no deadline's message and delivers nothing: a request names an
//! instance and no more. It writes nothing either, not even how far it has
//! read: the index is a cache of the schedule facts, and every partition reads
//! them again from the start of the journal when it starts and whenever it is
//! rebuilt. What that costs is time proportional to the schedule and lifecycle
//! facts, because the store is asked for those two event types alone.
//!
//! # Which facts are this saga type's
//!
//! Every saga type's schedule facts share one event type, and what tells this
//! type's apart is the deadline message type each names — see
//! [`DeclaresDeadline::MESSAGE_TYPE`](crate::DeclaresDeadline::MESSAGE_TYPE).
//!
//! # Asking is at-least-once
//!
//! A due deadline is asked for on every turn until the journal says it is
//! over — its `fired` or `cancelled` fact, or the saga's `ended` fact. A
//! resident instance makes a request a no-op, so the repeats cost a message.

mod index;
mod process;
mod proxy;

use std::num::NonZeroUsize;
use std::time::Duration;

use nitinol_persistence::store::Journal;
use nitinol_runtime::process::{ProcessProxy, SupervisionStrategy};
use nitinol_runtime::{IdleTimeout, ProcessSystem, Props};

use crate::deadline_index::process::{IndexDriver, SagaDeadlineIndexPartition};
use crate::delivery::{RESTART_MAX_RETRIES, RESTART_WITHIN};
use crate::process::manager::SagaManagerProcess;
use crate::saga::Saga;
use crate::subscribe::{DeclaresDeadline, SagaSubscribe};

pub use self::proxy::SagaDeadlineIndexProxy;

/// Start the deadline index of `S` on `system`, over `journal`, asking
/// `manager` for the instances whose deadlines fall due — or nothing, when
/// `S`'s alphabet declares no deadline.
///
/// `partitions` and `poll_interval` are the delivery host's, so the index
/// assigns a saga to the same partition the delivery host does.
pub(crate) async fn start_for<S: Saga>(
    partitions: NonZeroUsize,
    poll_interval: Duration,
    journal: Journal,
    manager: ProcessProxy<SagaManagerProcess<S>>,
    system: &ProcessSystem,
) -> Option<SagaDeadlineIndexProxy> {
    let message_type = <<S::Subscribe as SagaSubscribe>::Deadline as DeclaresDeadline<
        S::Subscribe,
    >>::MESSAGE_TYPE?;

    let mut pids = Vec::with_capacity(partitions.get());
    for partition in 0..partitions.get() {
        let journal = journal.clone();
        let manager = manager.clone();

        let props = Props::new(move || {
            // Nothing of what the partition had read is captured here: a
            // rebuilt partition starts with an empty index at the start of the
            // journal, which is what makes losing it cost only time.
            SagaDeadlineIndexPartition::<S>::new(
                message_type,
                partition,
                partitions,
                journal.clone(),
                manager.clone(),
            )
        })
        .with_supervision_strategy(
            SupervisionStrategy::restart(RESTART_MAX_RETRIES, RESTART_WITHIN)
                .expect("the restart window is a positive duration constant"),
        )
        // A quiet journal is an index with nothing due, not one to stop:
        // nothing would be left to revive it.
        .with_idle_timeout(IdleTimeout::Persistent)
        .with_driver(IndexDriver::<S>::new(poll_interval));

        pids.push(system.spawn(props).await.pid());
    }

    Some(SagaDeadlineIndexProxy::new(pids))
}
