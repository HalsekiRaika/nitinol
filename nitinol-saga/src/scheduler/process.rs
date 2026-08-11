//! `SchedulerProcess` — the system-wide resident timer core.
//!
//! A single non-generic process owns every saga's timers using a **Hashed Wheel
//! Timer** with [`WHEEL_SIZE`] slots and a [`TICK_INTERVAL`] resolution.  It
//! stays saga-type agnostic by keying registrations on [`ScheduleToken`] and
//! dispatching through a type-erased [`DispatchFn`] that the saga builds while
//! it still has access to its own typed proxy.
//!
//! ## Timer precision
//!
//! Timers fire within one tick period (10 ms) after their deadline.  The wheel
//! has 512 slots, giving a maximum non-round-trip delay of 5.12 s per
//! revolution; longer delays are handled by a `remaining_rounds` counter.
//!
//! ## Cancellation
//!
//! A per-token monotonic generation counter stored in `generations` is the
//! authoritative active-timer table.  Cancelling a token removes it from
//! `generations`; existing wheel entries carrying a stale generation are
//! discarded lazily when their slot is visited.  This makes `CancelTimer` and
//! `CancelAllForSaga` O(1) (or O(active timers for saga), respectively) without
//! scanning the wheel.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use futures_core::future::BoxFuture;
use nitinol_runtime::process::{Process, ProcessContext, ProcessProxy, Receive};
use nitinol_runtime::{IdleTimeout, ProcessSystem, Props};

use crate::id::SagaId;
use crate::scheduler::token::ScheduleToken;

/// Type-erased dispatch invoked when a timer fires.  The saga constructs this
/// by capturing a clone of its own typed proxy, so the scheduler never needs
/// to know the concrete saga type.
pub(crate) type DispatchFn = Arc<dyn Fn() -> BoxFuture<'static, ()> + Send + Sync>;

/// Number of slots in the hashed wheel.  Must be a power of two for the modulo
/// to simplify to a bitwise AND, though we use plain `%` for clarity.
const WHEEL_SIZE: usize = 512;

/// How long between wheel ticks.  Timers fire within `TICK_INTERVAL` of their
/// deadline.
const TICK_INTERVAL: Duration = Duration::from_millis(10);

/// Convert a [`Duration`] into the number of ticks the wheel must advance
/// before the timer may fire.
///
/// Uses ceiling division so the timer **never fires before `after` has
/// elapsed**:
///
/// | `after`  | ticks |
/// |----------|-------|
/// | 0 ms     | 1     |
/// | 10 ms    | 1     |
/// | 11 ms    | 2     |
/// | 19 ms    | 2     |
/// | 20 ms    | 2     |
/// | 21 ms    | 3     |
///
/// The minimum is 1 so the entry always lands in a *future* slot and never
/// the current one.  Saturates to [`u64::MAX`] on pathological durations
/// (≫ 5 000 years).
fn duration_to_ticks(after: Duration) -> u64 {
    let nanos = after.as_nanos();
    if nanos == 0 {
        return 1;
    }
    let tick_nanos = TICK_INTERVAL.as_nanos();
    // Ceiling division keeps the entry in a future slot; flooring could fire early.
    let ticks = nanos.div_ceil(tick_nanos);
    u64::try_from(ticks).unwrap_or(u64::MAX)
}

/// A single pending timer entry held in a wheel slot.
struct WheelEntry {
    token: ScheduleToken,
    generation: u64,
    /// How many more full wheel revolutions must pass before this entry is
    /// eligible to fire.  Decremented once per visit to the slot; fires when
    /// it reaches zero.
    remaining_rounds: u64,
    dispatch: DispatchFn,
}

/// The resident timer core.  Spawn one per [`ProcessSystem`] via
/// [`spawn_scheduler`].
pub struct SchedulerProcess {
    /// The hashed wheel: `WHEEL_SIZE` slots, each a list of pending entries.
    wheel: Box<[Vec<WheelEntry>]>,
    /// Monotonically increasing tick counter (never wrapped).
    /// The slot processed on each tick is `current_tick % WHEEL_SIZE`.
    current_tick: u64,
    /// Authoritative active-generation table.  A token present here with the
    /// matching generation is live; absent or mismatched means stale.
    generations: HashMap<ScheduleToken, u64>,
}

impl SchedulerProcess {
    fn new() -> Self {
        let wheel = std::iter::repeat_with(Vec::new)
            .take(WHEEL_SIZE)
            .collect::<Vec<_>>()
            .into_boxed_slice();
        SchedulerProcess {
            wheel,
            current_tick: 0,
            generations: HashMap::new(),
        }
    }
}

impl Process for SchedulerProcess {
    async fn on_start(&mut self, ctx: &mut ProcessContext<Self>) {
        // Kick off the recurring wheel tick.
        ctx.pipe_to_self(async { tokio::time::sleep(TICK_INTERVAL).await }, |_| {
            WheelTick
        });
    }
}

/// Register (or replace) a single-shot timer for `token`, firing after `after`.
pub(crate) struct RegisterTimer {
    pub(crate) token: ScheduleToken,
    pub(crate) after: Duration,
    pub(crate) dispatch: DispatchFn,
}

/// Cancel the timer registered under `token`, if any.
pub(crate) struct CancelTimer {
    pub(crate) token: ScheduleToken,
}

/// Cancel every timer owned by `saga_id` (saga lifecycle teardown).
pub(crate) struct CancelAllForSaga {
    pub(crate) saga_id: SagaId,
}

/// Internal self-message that advances the wheel by one slot.
struct WheelTick;

impl Receive<RegisterTimer> for SchedulerProcess {
    type Response = ();
    type Error = std::convert::Infallible;

    async fn recv(
        &mut self,
        msg: RegisterTimer,
        _ctx: &mut ProcessContext<Self>,
    ) -> Result<(), std::convert::Infallible> {
        // Bump the generation so any earlier wheel entry for this token becomes stale.
        let generation = self.generations.get(&msg.token).map(|g| g + 1).unwrap_or(1);
        self.generations.insert(msg.token.clone(), generation);

        // Convert `after` to a number of ticks using ceiling division so the
        // timer never fires before the deadline (minimum 1 tick).
        let ticks_until = duration_to_ticks(msg.after);

        // Target slot: `(current_tick + ticks_until) % WHEEL_SIZE`.
        // Decompose into modular parts to avoid u64 overflow when `ticks_until`
        // is saturated to `u64::MAX` (pathological duration such as Duration::MAX).
        let wheel = WHEEL_SIZE as u64;
        let slot = (((self.current_tick % wheel) + (ticks_until % wheel)) % wheel) as usize;

        // remaining_rounds: how many full wheel revolutions before the entry
        // may fire.  The formula `(ticks_until - 1) / WHEEL_SIZE` ensures the
        // entry fires exactly `ticks_until` ticks from now (off-by-one
        // correction for the "advance before drain" tick loop design).
        let remaining_rounds = ticks_until.saturating_sub(1) / WHEEL_SIZE as u64;

        self.wheel[slot].push(WheelEntry {
            token: msg.token,
            generation,
            remaining_rounds,
            dispatch: msg.dispatch,
        });

        Ok(())
    }
}

impl Receive<CancelTimer> for SchedulerProcess {
    type Response = ();
    type Error = std::convert::Infallible;

    async fn recv(
        &mut self,
        msg: CancelTimer,
        _ctx: &mut ProcessContext<Self>,
    ) -> Result<(), std::convert::Infallible> {
        // Remove from the authoritative table; the wheel entry is discarded
        // lazily as stale when its slot is next visited.
        self.generations.remove(&msg.token);
        Ok(())
    }
}

impl Receive<CancelAllForSaga> for SchedulerProcess {
    type Response = ();
    type Error = std::convert::Infallible;

    async fn recv(
        &mut self,
        msg: CancelAllForSaga,
        _ctx: &mut ProcessContext<Self>,
    ) -> Result<(), std::convert::Infallible> {
        self.generations
            .retain(|token, _| token.saga_id != msg.saga_id);
        Ok(())
    }
}

impl Receive<WheelTick> for SchedulerProcess {
    type Response = ();
    type Error = std::convert::Infallible;

    async fn recv(
        &mut self,
        _: WheelTick,
        ctx: &mut ProcessContext<Self>,
    ) -> Result<(), std::convert::Infallible> {
        // Advance the wheel pointer.
        self.current_tick += 1;
        let slot = (self.current_tick % WHEEL_SIZE as u64) as usize;

        // Drain the slot; entries not yet ready are re-queued in the same slot
        // for the next revolution.
        let entries = std::mem::take(&mut self.wheel[slot]);
        let mut to_requeue: Vec<WheelEntry> = Vec::new();

        for mut entry in entries {
            // Discard stale entries: cancelled tokens or superseded registrations.
            if self.generations.get(&entry.token) != Some(&entry.generation) {
                continue;
            }

            if entry.remaining_rounds > 0 {
                entry.remaining_rounds -= 1;
                to_requeue.push(entry);
            } else {
                // Fire: remove from the authoritative table first (best-effort
                // once — prevents a stale duplicate from firing the same
                // registration twice).
                self.generations.remove(&entry.token);
                (entry.dispatch)().await;
            }
        }

        self.wheel[slot].extend(to_requeue);

        // Re-arm the next tick.
        ctx.pipe_to_self(async { tokio::time::sleep(TICK_INTERVAL).await }, |_| {
            WheelTick
        });

        Ok(())
    }
}

/// A cloneable handle to the resident [`SchedulerProcess`].
///
/// Injected into a saga via [`crate::SagaProps::with_scheduler`].  The methods
/// are the named timer operations the saga effect interpreter and lifecycle
/// hooks drive.
#[derive(Clone)]
pub struct SchedulerProxy {
    inner: ProcessProxy<SchedulerProcess>,
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

/// Spawn the system-wide resident [`SchedulerProcess`] and return a proxy to it.
///
/// The process is persistent (never idle-times-out) because it must outlive
/// individual sagas to keep their timers running.
pub async fn spawn_scheduler(system: &ProcessSystem) -> SchedulerProxy {
    let props = Props::new(SchedulerProcess::new).with_idle_timeout(IdleTimeout::Persistent);
    let inner = system.spawn(props).await;
    SchedulerProxy { inner }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression tests for `duration_to_ticks`.
    ///
    /// The contract: a timer registered with `after` must NOT fire before
    /// `after` has elapsed.  This requires **ceiling** division, not floor.
    /// The cases below cover zero, exact-tick boundaries, and non-aligned
    /// values that previously produced wrong tick counts with floor division.
    #[test]
    fn duration_to_ticks_never_fires_before_deadline() {
        let tick_ms = u64::try_from(TICK_INTERVAL.as_millis())
            .expect("tick interval in milliseconds must fit in u64");

        // (after_ms, expected_ticks, description)
        let cases: &[(u64, u64, &str)] = &[
            (0, 1, "0ms → min 1 tick"),
            (1, 1, "1ms → 1 tick (fires after 10ms, still safe)"),
            (9, 1, "9ms → 1 tick (fires after 10ms, safe)"),
            (10, 1, "10ms → exactly 1 tick"),
            (
                11,
                2,
                "11ms → 2 ticks (was 1 with floor division — regression)",
            ),
            (
                19,
                2,
                "19ms → 2 ticks (was 1 with floor division — regression)",
            ),
            (20, 2, "20ms → exactly 2 ticks"),
            (21, 3, "21ms → 3 ticks"),
        ];

        for &(after_ms, expected_ticks, desc) in cases {
            let ticks = duration_to_ticks(Duration::from_millis(after_ms));
            assert_eq!(ticks, expected_ticks, "{desc}");

            // Core invariant: the timer fires no earlier than `after` ms.
            let fire_ms = ticks * tick_ms;
            assert!(
                fire_ms >= after_ms,
                "{desc}: fire_ms={fire_ms} < after_ms={after_ms} — deadline violated!"
            );
        }
    }

    /// Regression test: slot calculation must not overflow when
    /// `ticks_until` is saturated to [`u64::MAX`] (i.e., `Duration::MAX`).
    ///
    /// Before the fix, `self.current_tick + ticks_until` would overflow for any
    /// `current_tick > 0`.  After the fix, modular decomposition is used:
    /// `(current_tick % wheel + ticks_until % wheel) % wheel`.
    #[test]
    fn slot_calculation_does_not_overflow_with_max_duration() {
        // duration_to_ticks(Duration::MAX) must saturate to u64::MAX.
        let ticks_until = duration_to_ticks(Duration::MAX);
        assert_eq!(
            ticks_until,
            u64::MAX,
            "Duration::MAX must saturate to u64::MAX"
        );

        // Slot calculation with current_tick > 0 must not panic.
        let current_tick: u64 = 1;
        let wheel = WHEEL_SIZE as u64;
        let slot = (((current_tick % wheel) + (ticks_until % wheel)) % wheel) as usize;
        assert!(slot < WHEEL_SIZE, "slot must be within wheel bounds");

        // remaining_rounds uses saturating_sub, so no overflow.
        let remaining_rounds = ticks_until.saturating_sub(1) / WHEEL_SIZE as u64;
        assert!(
            remaining_rounds > 0,
            "remaining_rounds must be non-zero for u64::MAX ticks"
        );
    }

    /// Verify the slot/rounds formula: an entry inserted `ticks_until` ticks
    /// before the first visit to its slot fires exactly at tick `ticks_until`.
    ///
    /// This test exercises the core arithmetic without spawning a real process,
    /// by simulating the wheel advance loop directly.
    #[test]
    fn wheel_slot_and_rounds_formula_is_correct() {
        // Pairs of (ticks_until, expected_fire_tick) to verify.
        let cases: &[(u64, u64)] = &[
            (1, 1),
            (WHEEL_SIZE as u64 - 1, WHEEL_SIZE as u64 - 1),
            (WHEEL_SIZE as u64, WHEEL_SIZE as u64),
            (WHEEL_SIZE as u64 + 1, WHEEL_SIZE as u64 + 1),
            (2 * WHEEL_SIZE as u64, 2 * WHEEL_SIZE as u64),
            (360_000, 360_000), // 1-hour timer at 10 ms/tick
        ];

        for &(ticks_until, expected_fire) in cases {
            let inserted_at: u64 = 0;
            let wheel = WHEEL_SIZE as u64;
            let slot = (((inserted_at % wheel) + (ticks_until % wheel)) % wheel) as usize;
            let remaining_rounds = ticks_until.saturating_sub(1) / WHEEL_SIZE as u64;

            // Simulate wheel advancement: find when remaining_rounds reaches 0
            // and the slot is visited.
            let mut current_tick: u64 = 0;
            let mut rounds = remaining_rounds;
            let fire_tick = loop {
                current_tick += 1;
                if (current_tick % WHEEL_SIZE as u64) as usize == slot {
                    if rounds == 0 {
                        break current_tick;
                    }
                    rounds -= 1;
                }
            };

            assert_eq!(
                fire_tick, expected_fire,
                "ticks_until={ticks_until}: expected fire at tick {expected_fire}, \
                 got {fire_tick} (slot={slot}, rounds={remaining_rounds})"
            );
        }
    }
}
