//! `SchedulerProcess` — the system-wide resident timer core.
//!
//! A single non-generic process owns every saga's deadlines, held in **deadline
//! order** and waited on one at a time.  It stays saga-type agnostic by keying
//! registrations on [`ScheduleToken`] and firing through a type-erased
//! [`DispatchFn`] the saga builds while it still has access to its own route
//! home.
//!
//! ## What the order costs
//!
//! Only the earliest deadline is ever waited on, with a single
//! [`tokio::time::sleep_until`].  So:
//!
//! - an idle scheduler costs nothing: no deadline, no sleep, no wakeup;
//! - a deadline arbitrarily far ahead is just a later key — there are no
//!   revolutions to count and no horizon to exceed;
//! - a registration is `O(log n)`, which is the trade this domain is happy to
//!   make: saga deadlines are few and long-lived, not millions of short-lived
//!   network timeouts.
//!
//! ## Cancellation
//!
//! [`SchedulerProcess::placed`] says where each token's deadline sits, so
//! cancelling one — by name, or by saga at lifecycle teardown — is a lookup and
//! a removal rather than a scan, and nothing stale is left in the order to be
//! filtered out later.
//!
//! ## The scheduler holds no truth
//!
//! Where a deadline durably lives is the saga's own stream: the `scheduled`
//! marker is written in the same atomic batch as the decision that set it.
//! Everything here is an accelerator over those facts, which is why starting
//! this process restores nothing — re-arming belongs to each instance's
//! activation.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use futures_core::future::BoxFuture;
use futures_util::future::select;
use nitinol_runtime::process::{Process, ProcessContext, ProcessProxy, Receive};
use nitinol_runtime::{Props, Resident};
use tokio::sync::oneshot;
use tokio::time::Instant;

use crate::id::SagaId;
use crate::scheduler::token::ScheduleToken;

/// Type-erased dispatch invoked when a deadline is reached.  The saga
/// constructs this by capturing the route back to itself, so the scheduler
/// never needs to know the concrete saga type.
pub(crate) type DispatchFn = Arc<dyn Fn() -> BoxFuture<'static, ()> + Send + Sync>;

/// Where a deadline is placed when its delay runs past what the monotonic
/// clock can represent.
///
/// `tokio`'s own timer treats roughly this distance as "never", so a delay
/// this long is already indistinguishable from one that is never reached; what
/// matters is that an absurd [`Duration`] leaves the one timer core standing
/// instead of overflowing it.
const FAR_FUTURE: Duration = Duration::from_secs(30 * 365 * 24 * 60 * 60);

/// A deadline's place in the firing order.
///
/// The instant decides it; the serial number only keeps two deadlines due at
/// the same instant from displacing one another.
type DeadlineKey = (Instant, u64);

/// A deadline waiting to be reached.
struct Pending {
    token: ScheduleToken,
    dispatch: DispatchFn,
}

/// The one sleep this process ever waits on.
///
/// Exactly one is outstanding at a time, which is what [`Sleep::Ending`] is
/// for: a sleep is never started while another is on its way back, however
/// often the order changes in between.
enum Sleep {
    /// Nothing is waited for.  An idle scheduler rests here — no deadline, no
    /// sleep, no wakeup.
    Idle,
    /// A sleep is outstanding for this deadline.  Holding the cancellation
    /// sender is what lets a nearer deadline end it: dropping the sender
    /// resolves the sleep at once.
    Waiting {
        deadline: Instant,
        /// Held, never read: nothing is sent on it, and dropping it is the
        /// whole signal.
        _cancel: oneshot::Sender<()>,
    },
    /// The outstanding sleep has been told to end, and its wakeup is already
    /// on the way.  That wakeup starts whatever sleep the order needs by then,
    /// so nothing starts one in the meantime.
    Ending,
}

/// What a change to the order asks of the one sleep.
enum SleepChange {
    /// Leave the outstanding sleep — or the absence of one — as it is.
    Leave,
    /// Start a sleep for this deadline.
    StartFor(Instant),
    /// End the outstanding sleep; its wakeup starts whatever is needed then.
    End,
}

/// The resident timer core.  Resolved by type through
/// [`ProcessSystem::spawn_resident`](nitinol_runtime::ProcessSystem::spawn_resident),
/// so a system holds exactly one however many sagas ask for it.
pub struct SchedulerProcess {
    /// Every registered deadline, in firing order.
    deadlines: BTreeMap<DeadlineKey, Pending>,
    /// Where each registered token's deadline sits in that order.
    placed: HashMap<ScheduleToken, DeadlineKey>,
    /// Serial numbers for [`DeadlineKey`], handed out in registration order.
    next_serial: u64,
    sleep: Sleep,
}

impl SchedulerProcess {
    fn new() -> Self {
        SchedulerProcess {
            deadlines: BTreeMap::new(),
            placed: HashMap::new(),
            next_serial: 0,
            sleep: Sleep::Idle,
        }
    }

    /// Place a single-shot deadline for `token`, superseding any earlier
    /// registration under the same token.
    fn insert(&mut self, token: ScheduleToken, after: Duration, dispatch: DispatchFn) {
        // Replacing rather than layering: `startSingleTimer` semantics say the
        // name holds one deadline, and leaving the earlier one in the order
        // would fire it as well.
        self.remove(&token);

        let key = (deadline_in(after), self.next_serial);
        self.next_serial += 1;
        self.placed.insert(token.clone(), key);
        self.deadlines.insert(key, Pending { token, dispatch });
    }

    /// Drop the deadline registered under `token`, if there is one.
    fn remove(&mut self, token: &ScheduleToken) {
        if let Some(key) = self.placed.remove(token) {
            self.deadlines.remove(&key);
        }
    }

    /// Drop every deadline `saga_id` owns.
    fn remove_all_owned_by(&mut self, saga_id: &SagaId) {
        let owned: Vec<ScheduleToken> = self
            .placed
            .keys()
            .filter(|token| &token.saga_id == saga_id)
            .cloned()
            .collect();
        for token in owned {
            self.remove(&token);
        }
    }

    /// Take every deadline that has been reached, earliest first, leaving the
    /// rest in the order.
    fn take_due(&mut self) -> Vec<Pending> {
        // `u64::MAX` as the serial puts the split past every deadline due at
        // this instant, so one registered for exactly now is taken rather than
        // held back for a wakeup that nothing would arm.
        let later = self.deadlines.split_off(&(Instant::now(), u64::MAX));
        let due = std::mem::replace(&mut self.deadlines, later);
        let due: Vec<Pending> = due.into_values().collect();
        for pending in &due {
            self.placed.remove(&pending.token);
        }
        due
    }

    /// The deadline at the head of the order, if the order holds one.
    fn earliest(&self) -> Option<Instant> {
        self.deadlines.keys().next().map(|(at, _)| *at)
    }

    /// What the one sleep must become for the current order.
    fn sleep_change(&self) -> SleepChange {
        match (&self.sleep, self.earliest()) {
            // A sleep is already on its way back; its wakeup settles this.
            (Sleep::Ending, _) => SleepChange::Leave,
            // Nothing to wait for and nothing waiting: an idle scheduler.
            (Sleep::Idle, None) => SleepChange::Leave,
            (Sleep::Idle, Some(deadline)) => SleepChange::StartFor(deadline),
            // Waking before the earliest deadline costs one wakeup that fires
            // nothing and starts the right sleep; ending this one costs the
            // same wakeup, so it is left where it is.
            (Sleep::Waiting { deadline, .. }, Some(earliest)) if *deadline <= earliest => {
                SleepChange::Leave
            }
            // Either a nearer deadline arrived or the last one went away.
            (Sleep::Waiting { .. }, _) => SleepChange::End,
        }
    }

    /// Make the one sleep match the order: start it, end it, or leave it.
    fn settle_sleep(&mut self, ctx: &mut ProcessContext<Self>) {
        match self.sleep_change() {
            SleepChange::Leave => {}
            // Assigning drops the cancellation sender, which resolves the
            // outstanding sleep rather than leaving it to wake for a deadline
            // nobody waits on any more.
            SleepChange::End => self.sleep = Sleep::Ending,
            SleepChange::StartFor(deadline) => self.start_sleep(deadline, ctx),
        }
    }

    /// Wait for `deadline`, or until the order ends the wait.
    fn start_sleep(&mut self, deadline: Instant, ctx: &mut ProcessContext<Self>) {
        let (cancel, cancelled) = oneshot::channel::<()>();
        self.sleep = Sleep::Waiting {
            deadline,
            _cancel: cancel,
        };
        ctx.pipe_to_self(
            async move {
                // Whichever resolves first, the process has to re-read the
                // order: the deadline arrived, or the order moved under it.
                select(Box::pin(tokio::time::sleep_until(deadline)), cancelled).await;
            },
            |outcome| {
                // Neither a sleep nor a dropped cancellation can panic.
                // Reporting it and re-reading the order is what would keep
                // every other saga's deadline alive if one ever did.
                if let Err(panic) = outcome {
                    tracing::error!(
                        panic = %panic,
                        "scheduler sleep panicked; re-reading the deadline order"
                    );
                }
                Wakeup
            },
        );
    }
}

/// The instant `after` from now.
///
/// A delay the monotonic clock cannot represent is placed at [`FAR_FUTURE`]
/// rather than overflowing: the registration is absurd, but it is one saga's
/// absurdity and every other saga's deadlines live in this process too.
fn deadline_in(after: Duration) -> Instant {
    let now = Instant::now();
    match now.checked_add(after) {
        Some(deadline) => deadline,
        None => {
            tracing::warn!(
                ?after,
                "schedule delay runs past the monotonic clock's range; \
                 placing the deadline as far ahead as the clock allows"
            );
            now + FAR_FUTURE
        }
    }
}

impl Process for SchedulerProcess {}

impl Resident for SchedulerProcess {
    /// Nothing to restore and nothing to arm: a scheduler started after a
    /// restart is empty, and each saga instance re-arms its own deadlines when
    /// it is activated.
    fn props() -> Props<Self> {
        Props::new(SchedulerProcess::new)
    }
}

/// Register (or replace) a single-shot deadline for `token`, reached after
/// `after`.
pub(crate) struct RegisterTimer {
    pub(crate) token: ScheduleToken,
    pub(crate) after: Duration,
    pub(crate) dispatch: DispatchFn,
}

/// Cancel the deadline registered under `token`, if any.
pub(crate) struct CancelTimer {
    pub(crate) token: ScheduleToken,
}

/// Cancel every deadline owned by `saga_id` (saga lifecycle teardown).
pub(crate) struct CancelAllForSaga {
    pub(crate) saga_id: SagaId,
}

/// Internal self-message delivered when the one sleep ends — reached or
/// ended early, which the order answers for itself.
struct Wakeup;

impl Receive<RegisterTimer> for SchedulerProcess {
    type Response = ();
    type Error = std::convert::Infallible;

    async fn recv(
        &mut self,
        msg: RegisterTimer,
        ctx: &mut ProcessContext<Self>,
    ) -> Result<(), std::convert::Infallible> {
        self.insert(msg.token, msg.after, msg.dispatch);
        self.settle_sleep(ctx);
        Ok(())
    }
}

impl Receive<CancelTimer> for SchedulerProcess {
    type Response = ();
    type Error = std::convert::Infallible;

    async fn recv(
        &mut self,
        msg: CancelTimer,
        ctx: &mut ProcessContext<Self>,
    ) -> Result<(), std::convert::Infallible> {
        self.remove(&msg.token);
        self.settle_sleep(ctx);
        Ok(())
    }
}

impl Receive<CancelAllForSaga> for SchedulerProcess {
    type Response = ();
    type Error = std::convert::Infallible;

    async fn recv(
        &mut self,
        msg: CancelAllForSaga,
        ctx: &mut ProcessContext<Self>,
    ) -> Result<(), std::convert::Infallible> {
        self.remove_all_owned_by(&msg.saga_id);
        self.settle_sleep(ctx);
        Ok(())
    }
}

impl Receive<Wakeup> for SchedulerProcess {
    type Response = ();
    type Error = std::convert::Infallible;

    async fn recv(
        &mut self,
        _: Wakeup,
        ctx: &mut ProcessContext<Self>,
    ) -> Result<(), std::convert::Infallible> {
        // However it ended, that sleep is over.
        self.sleep = Sleep::Idle;
        // Whatever has been reached is due whether the sleep was waiting for
        // it or was ended early by a nearer one, so both read the order the
        // same way.
        let due = self.take_due();
        // Settled before any dispatch runs, so a slow delivery cannot hold up
        // the deadlines behind it.
        self.settle_sleep(ctx);

        // A dispatch only sends a message elsewhere and cannot re-enter the
        // scheduler while this `recv` holds `&mut self`, so it can never
        // observe the order mid-change.
        for pending in due {
            (pending.dispatch)().await;
        }

        Ok(())
    }
}

/// A cloneable handle to the resident [`SchedulerProcess`].
///
/// Obtained by converting the proxy
/// [`ProcessSystem::spawn_resident`](nitinol_runtime::ProcessSystem::spawn_resident)
/// hands back, which the spawn boundary does for a saga whose alphabet declares a
/// deadline — the only place one is made.  It stays with the hosting machinery and
/// never reaches a saga: the methods are the named timer operations the saga
/// effect interpreter and lifecycle hooks drive, *after* the fact behind each has
/// been appended.
#[derive(Clone)]
pub struct SchedulerProxy {
    inner: ProcessProxy<SchedulerProcess>,
}

impl From<ProcessProxy<SchedulerProcess>> for SchedulerProxy {
    fn from(inner: ProcessProxy<SchedulerProcess>) -> Self {
        SchedulerProxy { inner }
    }
}

impl SchedulerProxy {
    pub(crate) async fn register(
        &self,
        token: ScheduleToken,
        after: Duration,
        dispatch: DispatchFn,
    ) {
        if let Err(e) = self
            .inner
            .tell(RegisterTimer {
                token,
                after,
                dispatch,
            })
            .await
        {
            tracing::warn!(error = ?e, "scheduler register timer failed");
        }
    }

    pub(crate) async fn cancel(&self, token: ScheduleToken) {
        if let Err(e) = self.inner.tell(CancelTimer { token }).await {
            tracing::warn!(error = ?e, "scheduler cancel timer failed");
        }
    }

    pub(crate) async fn cancel_all(&self, saga_id: SagaId) {
        if let Err(e) = self.inner.tell(CancelAllForSaga { saga_id }).await {
            tracing::warn!(error = ?e, "scheduler cancel-all failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::scheduler::token::TimerName;

    fn token(saga: &str, name: &str) -> ScheduleToken {
        ScheduleToken {
            saga_id: SagaId::new(saga),
            name: TimerName::new(name),
        }
    }

    /// A dispatch that does nothing: these tests observe *which* deadlines the
    /// order hands back and when, never what a firing does.
    fn noop_dispatch() -> DispatchFn {
        Arc::new(|| -> BoxFuture<'static, ()> { Box::pin(async {}) })
    }

    fn register(scheduler: &mut SchedulerProcess, token: ScheduleToken, after: Duration) {
        scheduler.insert(token, after, noop_dispatch());
    }

    /// Put `token` at an exact place in the order, which registering by delay
    /// cannot express: `insert` hands out serials in call order.
    fn place(scheduler: &mut SchedulerProcess, token: ScheduleToken, key: DeadlineKey) {
        scheduler.placed.insert(token.clone(), key);
        scheduler.deadlines.insert(
            key,
            Pending {
                token,
                dispatch: noop_dispatch(),
            },
        );
    }

    fn due_names(scheduler: &mut SchedulerProcess) -> Vec<String> {
        scheduler
            .take_due()
            .into_iter()
            .map(|pending| pending.token.name.as_str().to_owned())
            .collect()
    }

    /// Put the scheduler in the state it is in while waiting for `deadline`.
    ///
    /// The receiver is handed back because dropping it is what the production
    /// path reads as the sleep ending: a test that let it go would be
    /// observing a sleep that is already over.
    fn waiting_for(scheduler: &mut SchedulerProcess, deadline: Instant) -> oneshot::Receiver<()> {
        let (cancel, cancelled) = oneshot::channel::<()>();
        scheduler.sleep = Sleep::Waiting {
            deadline,
            _cancel: cancel,
        };
        cancelled
    }

    /// A scheduler holding no deadline starts no sleep, so no wakeup can
    /// happen while it is idle — the cost the retired wheel's 10 ms tick paid
    /// whether or not anything was registered.
    #[test]
    fn an_idle_scheduler_starts_no_sleep() {
        let scheduler = SchedulerProcess::new();

        assert!(
            matches!(scheduler.sleep_change(), SleepChange::Leave),
            "a scheduler with no deadline and no sleep must ask for neither"
        );
    }

    /// Cancelling the last deadline ends the outstanding sleep rather than
    /// leaving it to wake for a deadline that is gone.
    #[test]
    fn cancelling_the_last_deadline_ends_the_sleep() {
        let mut scheduler = SchedulerProcess::new();
        register(
            &mut scheduler,
            token("saga", "only"),
            Duration::from_secs(60),
        );
        let _cancelled = waiting_for(&mut scheduler, deadline_in(Duration::from_secs(60)));

        scheduler.remove(&token("saga", "only"));

        assert!(
            matches!(scheduler.sleep_change(), SleepChange::End),
            "with the order empty the scheduler must stop waiting at all"
        );
    }

    /// A deadline nearer than the one being waited for ends that sleep, so the
    /// nearer one is not missed.
    #[test]
    fn a_nearer_deadline_ends_the_outstanding_sleep() {
        let mut scheduler = SchedulerProcess::new();
        let _cancelled = waiting_for(&mut scheduler, deadline_in(Duration::from_secs(60)));

        register(&mut scheduler, token("saga", "sooner"), Duration::ZERO);

        assert!(
            matches!(scheduler.sleep_change(), SleepChange::End),
            "a deadline nearer than the sleep must not be left to a wakeup 60s away"
        );
    }

    /// A sleep that has already been told to end settles the order itself when
    /// its wakeup lands, so nothing starts a second one in the meantime —
    /// which is what keeps exactly one outstanding however often the order
    /// changes.
    #[test]
    fn a_sleep_on_its_way_back_is_not_joined_by_another() {
        let mut scheduler = SchedulerProcess::new();
        scheduler.sleep = Sleep::Ending;

        register(&mut scheduler, token("saga", "sooner"), Duration::ZERO);

        assert!(
            matches!(scheduler.sleep_change(), SleepChange::Leave),
            "a deadline arriving while a wakeup is on the way must wait for it"
        );
    }

    /// Only the deadlines that have been reached are handed back; the rest keep
    /// their place in the order.
    #[test]
    fn only_the_deadlines_that_came_due_are_taken() {
        let mut scheduler = SchedulerProcess::new();
        register(&mut scheduler, token("saga", "now"), Duration::ZERO);
        register(
            &mut scheduler,
            token("saga", "later"),
            Duration::from_secs(60),
        );

        assert_eq!(
            due_names(&mut scheduler),
            vec!["now".to_owned()],
            "a deadline already reached is due; one 60s out is not"
        );
        assert!(
            matches!(scheduler.sleep_change(), SleepChange::StartFor(_)),
            "the deadline left behind must still be waited for"
        );
    }

    /// Deadlines are handed back earliest first, whatever order they were
    /// registered in.
    #[test]
    fn due_deadlines_come_back_in_deadline_order() {
        let mut scheduler = SchedulerProcess::new();
        let now = Instant::now();
        let a_second_ago = now
            .checked_sub(Duration::from_secs(1))
            .expect("the monotonic clock has been running for at least a second");

        // The earlier deadline carries the *higher* serial, so registration
        // order and deadline order disagree and only the deadline can produce
        // the expectation below.
        place(&mut scheduler, token("saga", "second"), (now, 0));
        place(&mut scheduler, token("saga", "first"), (a_second_ago, 1));

        assert_eq!(
            due_names(&mut scheduler),
            vec!["first".to_owned(), "second".to_owned()],
            "the earlier deadline must be handed back first"
        );
    }

    /// Re-registering a name replaces the deadline held under it instead of
    /// adding a second one that would fire as well.
    #[test]
    fn registering_a_name_again_replaces_its_deadline() {
        let mut scheduler = SchedulerProcess::new();
        register(
            &mut scheduler,
            token("saga", "reminder"),
            Duration::from_secs(60),
        );
        register(&mut scheduler, token("saga", "reminder"), Duration::ZERO);

        assert_eq!(
            due_names(&mut scheduler),
            vec!["reminder".to_owned()],
            "the new deadline is due and the one it replaced must be gone"
        );
        assert!(
            matches!(scheduler.sleep_change(), SleepChange::Leave),
            "nothing of the replaced 60s deadline may be left in the order"
        );
    }

    /// Cancelling by name takes that deadline out and leaves the saga's others
    /// alone.
    #[test]
    fn cancelling_by_name_takes_out_only_that_deadline() {
        let mut scheduler = SchedulerProcess::new();
        register(&mut scheduler, token("saga", "cancelled"), Duration::ZERO);
        register(&mut scheduler, token("saga", "kept"), Duration::ZERO);

        scheduler.remove(&token("saga", "cancelled"));

        assert_eq!(
            due_names(&mut scheduler),
            vec!["kept".to_owned()],
            "a cancelled deadline must not be handed back when its instant arrives"
        );
    }

    /// Lifecycle teardown releases one saga's deadlines without touching
    /// another saga's, even where both used the same timer name.
    #[test]
    fn cancelling_a_saga_leaves_another_sagas_deadlines() {
        let mut scheduler = SchedulerProcess::new();
        register(
            &mut scheduler,
            token("stopping", "reminder"),
            Duration::ZERO,
        );
        register(&mut scheduler, token("running", "reminder"), Duration::ZERO);

        scheduler.remove_all_owned_by(&SagaId::new("stopping"));

        let due = scheduler.take_due();
        let owners: Vec<&str> = due
            .iter()
            .map(|pending| pending.token.saga_id.as_str())
            .collect();
        assert_eq!(
            owners,
            vec!["running"],
            "cancelling one saga's deadlines must be scoped to that saga"
        );
    }

    /// A delay no monotonic clock can represent must leave the one timer core
    /// standing: it is placed far ahead rather than overflowing the add.
    #[test]
    fn a_delay_past_the_clocks_range_is_placed_far_ahead() {
        let mut scheduler = SchedulerProcess::new();
        register(&mut scheduler, token("saga", "absurd"), Duration::MAX);

        assert!(
            due_names(&mut scheduler).is_empty(),
            "a deadline that far ahead must not be treated as already reached"
        );
    }

    /// A deadline hours out is a later key and nothing else — no revolution
    /// counting, and no horizon past which it is lost.
    #[test]
    fn a_deadline_hours_out_is_held_like_any_other() {
        let mut scheduler = SchedulerProcess::new();
        register(
            &mut scheduler,
            token("saga", "hours"),
            Duration::from_secs(6 * 60 * 60),
        );

        assert!(
            due_names(&mut scheduler).is_empty(),
            "a deadline six hours out must not come due now"
        );
        assert!(
            matches!(scheduler.sleep_change(), SleepChange::StartFor(_)),
            "it must still be the deadline the scheduler waits for"
        );
    }
}
