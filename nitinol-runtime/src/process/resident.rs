//! Type-keyed resident singletons.
//!
//! A [`Resident`] names its own way of starting; [`ResidentTable`] is what makes
//! at most one of it exist per system. Only the mechanism lives here — which
//! processes are residents is the declaring crate's decision, so the runtime
//! knows no resident type by name.

use std::any::TypeId;
use std::collections::HashMap;
use std::sync::Mutex;

use tokio::sync::watch;

use crate::process::spawn::SpawnEnv;
use crate::process::{AnyProxy, IdleTimeout, Process, ProcessProxy, Props};

/// A [`Process`] that declares how it is started, so the system can hold exactly
/// one of it per type.
///
/// [`props`][Self::props] is the whole contract: the type states its own
/// lifecycle, and
/// [`ProcessSystem::spawn_resident`](crate::ProcessSystem::spawn_resident) keys
/// on the type to start that lifecycle at most once.
///
/// Idle timeouts are not a resident's to decide. A resident is persistent, and
/// `spawn_resident` settles that over whatever `props` returned, so an
/// implementor has no reason to reach for
/// [`Props::with_idle_timeout`](crate::Props::with_idle_timeout).
pub trait Resident: Process + Sized {
    /// The lifecycle to start the first time this type is asked for.
    fn props() -> Props<Self>;
}

/// The one-per-type table behind
/// [`ProcessSystem::spawn_resident`](crate::ProcessSystem::spawn_resident).
pub(crate) struct ResidentTable {
    /// Each type's start, keyed by its own [`TypeId`].
    ///
    /// The value is a `watch::Receiver` rather than the finished proxy itself:
    /// the slot is claimed the instant the first ask arrives, before the
    /// lifecycle behind it has actually started, so a second ask arriving while
    /// the first is still starting waits on the same channel instead of racing
    /// it into a second start.
    ///
    /// Claiming the slot (the `HashMap` lookup/insert below) never spans an
    /// `.await`, so a std `Mutex` — not tokio's — is the right tool; nothing
    /// ever awaits while holding it.
    started: Mutex<HashMap<TypeId, watch::Receiver<Option<AnyProxy>>>>,
}

impl ResidentTable {
    pub(crate) fn new() -> Self {
        Self {
            started: Mutex::new(HashMap::new()),
        }
    }

    /// Resolve `P`'s resident, starting it when this is the first ask.
    pub(crate) async fn resolve<P: Resident>(&self, env: &SpawnEnv) -> ProcessProxy<P> {
        let mut receiver = {
            let mut started = self
                .started
                .lock()
                .expect("resident table mutex must not be poisoned");

            match started.get(&TypeId::of::<P>()) {
                Some(receiver) => receiver.clone(),
                None => {
                    let (sender, receiver) = watch::channel(None);
                    started.insert(TypeId::of::<P>(), receiver.clone());

                    let env = env.clone();
                    // Detached: the caller of `spawn_resident` can drop its own
                    // future at any point (a `select!`, a timeout), but that must
                    // not cut the start short — a half-registered resident would
                    // leave the slot claimed with nothing behind it, and a later
                    // ask would see the slot empty and start a second one.
                    // Running the start on its own task means only that task's
                    // completion, never an asker's cancellation, decides whether
                    // the resident ends up started.
                    //
                    // Persistence belongs to this call rather than to the
                    // declaration: a resident reaped for idling would leave every
                    // later ask resolving a process that no longer runs.
                    tokio::spawn(async move {
                        let proxy = env
                            .spawn(P::props().with_idle_timeout(IdleTimeout::Persistent))
                            .await;
                        let _ = sender.send(Some(proxy.into()));
                    });

                    receiver
                }
            }
        };

        // Cancellation-safe: dropping this `.await` drops only this asker's
        // interest in the outcome. The start task keeps running regardless, and
        // whoever asks next clones the same receiver and observes wherever it
        // left off.
        let proxy = receiver
            .wait_for(Option::is_some)
            .await
            .expect("the start task holds its sender until it sends Some")
            .clone()
            .expect("wait_for's predicate guarantees the value is Some");

        proxy
            .downcast::<P>()
            .expect("a resident slot holds the proxy of the type that keys it")
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use futures_util::FutureExt;

    use super::*;
    use crate::process::registry::ProcessRegistry;

    static SPAWNS: AtomicU32 = AtomicU32::new(0);

    struct Counted;
    impl Process for Counted {}
    impl Resident for Counted {
        fn props() -> Props<Self> {
            Props::new(|| {
                SPAWNS.fetch_add(1, Ordering::SeqCst);
                Counted
            })
        }
    }

    /// A caller of `spawn_resident` can drop its own future at any point (a
    /// `select!`, a `timeout()`). This pins the two halves of the guarantee
    /// that makes that safe: `resolve` must hand the start off to a task that
    /// survives the drop *before* it can return anything, and a later ask must
    /// still converge on that one lifecycle rather than starting a second one
    /// because the slot looked abandoned.
    #[tokio::test]
    async fn dropping_the_first_ask_after_its_first_poll_still_converges() {
        let table = ResidentTable::new();
        let env = SpawnEnv::detached(ProcessRegistry::new(), None);

        // Poll the first ask exactly once, dropping it immediately if that
        // poll does not finish it. A `resolve` that finished synchronously
        // here would have nothing left to cancel, so assert it does not: that
        // would mean this test is no longer exercising the drop at all.
        let first_poll = table.resolve::<Counted>(&env).now_or_never();
        assert!(
            first_poll.is_none(),
            "resolve must suspend on the detached start task rather than \
             finish on its first poll, otherwise dropping it here cancels \
             nothing"
        );

        // A second ask, made after the first was dropped, must still resolve
        // — and to the one lifecycle the dropped ask's detached task started.
        let _second = table.resolve::<Counted>(&env).await;

        assert_eq!(
            SPAWNS.load(Ordering::SeqCst),
            1,
            "cancelling the first ask must not let a later ask see an empty \
             slot and start a second lifecycle"
        );
    }
}
