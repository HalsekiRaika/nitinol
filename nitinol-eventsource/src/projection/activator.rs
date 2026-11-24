//! The process that keeps a system's projections and wakes their projectors.

mod declaration;
mod observer;
mod watcher;

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use nitinol_persistence::error::OffsetError;
use nitinol_persistence::EventType;
use nitinol_runtime::ident::Pid;
use nitinol_runtime::process::{Process, ProcessContext, ProcessProxy, Receive, Terminated};
use nitinol_runtime::{IdleTimeout, ProcessSystem, Props, SupervisionStrategy};
use tokio::sync::oneshot;

use crate::durable_stream::{
    DurableSubscription, PolledBatch, ReadReport, SequenceCursor, SettledThrough,
    DEFAULT_BATCH_LIMIT,
};
use crate::projection::ProjectionId;

pub(crate) use self::declaration::{Declaration, ProjectorDeclaration};
pub(crate) use self::observer::ActivatorObserver;
pub(crate) use self::watcher::{WatcherSetting, WatcherSource};

use self::declaration::{Interest, Running};
use self::observer::Appended;
use self::watcher::Seen;

/// The one process per system that holds every projection spawned on it, and
/// starts each one's projector whenever there is something for it to project.
///
/// - Every projector, and the poller that reads for it, is a child of the
///   activator, so a projector may stop on its idle timeout and lose nothing
///   by it: the activator keeps the declaration it was spawned with and builds
///   it anew from that the next time it is needed.
/// - What tells the activator that a projector is needed is an append. Appends
///   made through the system's own journal are reported to it as they are
///   accepted; appends made anywhere else are found by the watcher, which
///   reads the whole journal at a long interval. Both are handled alike: each
///   projection whose origin or handlers the appended events match is woken —
///   a running projector by having its poller read at once, a stopped one by
///   being activated again.
///
/// # Wake-ups are decided by position, not by time
///
/// For each projection the activator keeps two global positions: R, the
/// furthest event it has learned of that the projection follows, and M, how
/// far the projection has settled. An activation starts M where the
/// projection's recorded offset stands. When a projector stops, it is
/// activated again at once if R is past M.
///
/// That makes the following hold: an event the projection follows is
/// projected without waiting for another append. The activator learns of it
/// either before the projector stops — so R is past M when it stops, and it is
/// activated again — or after, when the projector is stopped and is activated
/// for it then. Reasoning by time instead would drop the event a poller had
/// just read past, learned of before an idle timeout and unread when it fired.
///
/// R is raised by the projection's own side as well, because no append
/// announced what lies in a projection's backlog from before it was spawned:
///
/// - The poller raises it to the last record of a batch before handing the
///   batch to the projector, so a projector that stopped before it was handed
///   the batch still leaves R past M.
/// - The projector raises M before it answers the batch that settled it, and
///   R for what the batch left owed. Those hold R past M across an idle stop,
///   so the activation that follows reads them again.
///
/// Until a read for the projection has reached the end of the journal and had
/// its batch answered by the projector, records may lie unread that neither R
/// nor an append accounts for: the reads failed, or came back full, or had not
/// yet run when the projector stopped. A projector that stops before that is
/// activated again once its poll interval has passed, which is when its poller
/// would have read again. An activation that fails is retried on a timer for
/// the same reason: the event it was for is not announced a second time.
pub(crate) struct ProjectionActivator {
    entries: HashMap<ProjectionId, Entry>,
    /// Projections following every stream, under each event type a handler of
    /// theirs takes.
    by_type: HashMap<EventType, Vec<ProjectionId>>,
    /// Projections following one stream, under that stream's key.
    by_stream: HashMap<Arc<str>, Vec<ProjectionId>>,
    /// Who is waiting for which projector, stopped through its handle, to end.
    stopping: HashMap<Pid, oneshot::Sender<()>>,
    watcher: Option<WatcherSource>,
}

/// One registered projection.
struct Entry {
    declaration: Box<dyn Declaration>,
    running: Option<Running>,
    /// R and M, shared with the projector: R is the furthest global position
    /// of an event this projection follows that the activator or the projector
    /// has learned of, M how far the projection has settled.
    progress: Progress,
    /// Whether an activation that failed is waiting to be tried again.
    retrying: bool,
}

/// How far through the journal, in global positions, a projection is known to
/// reach and has settled — R and M — and whether its poller has ever read to
/// the end of the journal and had what it returned dealt with.
///
/// Shared between the activator and the projector rather than reported in
/// messages: the activator weighs them as the projector's `Terminated`
/// arrives, and a system signal is handled ahead of a message sent before it,
/// so a reported value could be one batch behind.
#[derive(Clone, Default)]
pub(crate) struct Progress(Arc<Positions>);

#[derive(Default)]
struct Positions {
    known: AtomicU64,
    settled: AtomicU64,
    /// Set by the poller of any activation, and kept across activations.
    caught_up: AtomicBool,
}

impl Progress {
    /// Record that the projection follows an event at `position`; a position
    /// behind the one recorded leaves it as it is.
    pub(crate) fn learn_of(&self, position: u64) {
        self.0.known.fetch_max(position, Ordering::AcqRel);
    }

    /// Record that the projection has settled through `position`; a position
    /// behind the one recorded leaves it as it is.
    pub(crate) fn settle_through(&self, position: u64) {
        self.0.settled.fetch_max(position, Ordering::AcqRel);
    }

    /// Whether a read for this projection has ever reached the end of the
    /// journal and had what it returned dealt with.
    fn has_caught_up(&self) -> bool {
        self.0.caught_up.load(Ordering::Acquire)
    }

    fn settled(&self) -> u64 {
        self.0.settled.load(Ordering::Acquire)
    }

    /// Whether an event the projection follows lies past what it has settled.
    fn is_owed(&self) -> bool {
        self.0.known.load(Ordering::Acquire) > self.settled()
    }
}

/// A projection's poller reports its reads here: the records of a batch are
/// owed from the moment they are read, and the poller is caught up once a read
/// has gone to the end of the journal and been dealt with.
impl ReadReport for Progress {
    fn handing_over(&self, through: u64) {
        self.learn_of(through);
    }

    fn caught_up(&self) {
        self.0.caught_up.store(true, Ordering::Release);
    }
}

/// Register a projection and have it running; a projection already registered
/// under the same id keeps its declaration, and this joins it.
pub(crate) struct Register(pub(crate) Box<dyn Declaration>);

/// Take a projection out of the registry, stopping its projector if one runs.
pub(crate) struct Unregister(pub(crate) ProjectionId);

/// Whether a projection has a projector running.
pub(crate) struct IsActive(pub(crate) ProjectionId);

/// Start the watcher, now that its first interval has passed.
struct StartWatcher;

/// Try activating a projection again, after an attempt failed.
struct RetryActivation(ProjectionId);

/// Answered once the projector [`Unregister`] stopped has ended.
pub(crate) type Stopped = oneshot::Receiver<()>;

impl ProjectionActivator {
    /// Start a system's activator on `system`.
    ///
    /// Called from exactly one place: the memoised cell a system's node holds,
    /// on the first projector spawn. The system's journal reports its appends
    /// to whatever that cell holds, so from then on they reach this activator.
    /// It is persistent, because nothing would wake it, and resumes on a
    /// handler's error, because every projector in the system is its child.
    pub(crate) async fn spawn(
        system: &ProcessSystem,
        watcher: Option<WatcherSource>,
    ) -> ProcessProxy<Self> {
        let props = Props::new(move || ProjectionActivator {
            entries: HashMap::new(),
            by_type: HashMap::new(),
            by_stream: HashMap::new(),
            stopping: HashMap::new(),
            watcher: watcher.clone(),
        })
        .with_supervision_strategy(SupervisionStrategy::Resume)
        .with_idle_timeout(IdleTimeout::Persistent);
        system.spawn(props).await
    }

    fn entry(&mut self, id: &ProjectionId) -> &mut Entry {
        self.entries
            .get_mut(id)
            .expect("the activator names only projections it has registered")
    }

    /// Every registered projection that an append of `types` to the stream
    /// `key` is something to project for, each once.
    fn interested_in(&self, key: &str, types: &[EventType]) -> Vec<ProjectionId> {
        let by_stream = self.by_stream.get(key).into_iter().flatten();
        let by_type = types
            .iter()
            .filter_map(|event_type| self.by_type.get(event_type))
            .flatten();
        let mut ids: Vec<ProjectionId> = Vec::new();
        for id in by_stream.chain(by_type) {
            if !ids.contains(id) {
                ids.push(id.clone());
            }
        }
        ids
    }

    fn index(&mut self, id: &ProjectionId, interest: Interest) {
        match interest {
            Interest::EventTypes(types) => {
                for event_type in types {
                    self.by_type.entry(event_type).or_default().push(id.clone());
                }
            }
            Interest::Stream(key) => self.by_stream.entry(key).or_default().push(id.clone()),
        }
    }

    fn unindex(&mut self, id: &ProjectionId, interest: Interest) {
        fn remove<K>(index: &mut HashMap<K, Vec<ProjectionId>>, key: K, id: &ProjectionId)
        where
            K: std::hash::Hash + Eq,
        {
            if let Some(ids) = index.get_mut(&key) {
                ids.retain(|named| named != id);
                if ids.is_empty() {
                    index.remove(&key);
                }
            }
        }
        match interest {
            Interest::EventTypes(types) => {
                for event_type in types {
                    remove(&mut self.by_type, event_type, id);
                }
            }
            Interest::Stream(key) => remove(&mut self.by_stream, key, id),
        }
    }

    /// Start a projector for the registered projection `id`.
    async fn activate(
        &mut self,
        ctx: &mut ProcessContext<Self>,
        id: &ProjectionId,
    ) -> Result<(), OffsetError> {
        let entry = self.entry(id);
        let running = entry.declaration.activate(ctx, &entry.progress).await?;
        entry.running = Some(running);
        Ok(())
    }

    /// Activate `id` where nothing answering the caller depends on it. A
    /// failure is tried again after the projection's poll interval, however
    /// quiet the journal stays: an event the projection is owed will not be
    /// announced a second time.
    async fn reactivate(&mut self, ctx: &mut ProcessContext<Self>, id: &ProjectionId) {
        let Err(error) = self.activate(ctx, id).await else {
            return;
        };
        tracing::warn!(
            projection_id = ?id,
            error = %error,
            "projection activator: activating a projector failed; it is tried again \
             after the projection's poll interval"
        );
        self.activate_after_poll_interval(ctx, id);
    }

    /// Try activating `id` again once its poll interval has passed, unless a
    /// retry is already waiting.
    fn activate_after_poll_interval(&mut self, ctx: &ProcessContext<Self>, id: &ProjectionId) {
        let entry = self.entry(id);
        if entry.retrying {
            return;
        }
        entry.retrying = true;
        let retry = RetryActivation(id.clone());
        ctx.pipe_to_self(
            tokio::time::sleep(entry.declaration.poll_interval()),
            move |_slept| retry,
        );
    }

    /// Learn that the events up to global position `head`, of `types`, were
    /// appended to the stream `key`, and wake every projection that follows
    /// them and has not settled that far.
    async fn on_appended(
        &mut self,
        ctx: &mut ProcessContext<Self>,
        key: &str,
        head: u64,
        types: &[EventType],
    ) {
        for id in self.interested_in(key, types) {
            let entry = self.entry(&id);
            entry.progress.learn_of(head);
            if head <= entry.progress.settled() {
                continue;
            }
            match &entry.running {
                Some(running) => running.nudge.nudge(),
                None => self.reactivate(ctx, &id).await,
            }
        }
    }

    /// Start the watcher's first interval, after which it starts reading.
    ///
    /// Not at once: the watcher files its position in the projection slot, and
    /// a system that only briefly runs a projector has no use for a read of
    /// the whole journal at its start.
    fn schedule_watcher(ctx: &ProcessContext<Self>, source: &WatcherSource) {
        ctx.pipe_to_self(tokio::time::sleep(source.interval), |_slept| StartWatcher);
    }

    fn watcher(&self) -> &WatcherSource {
        self.watcher
            .as_ref()
            .expect("the watcher is started, and reads for the activator, only where one is set")
    }
}

impl Process for ProjectionActivator {
    async fn on_start(&mut self, ctx: &mut ProcessContext<Self>) {
        if let Some(source) = &self.watcher {
            Self::schedule_watcher(ctx, source);
        }
    }

    async fn on_terminated(&mut self, terminated: Terminated, ctx: &mut ProcessContext<Self>) {
        if let Some(stopped) = self.stopping.remove(&terminated.who) {
            // A stopper that gave up waiting has nothing to be told; the
            // projector has ended either way.
            let _ = stopped.send(());
        }
        let Some(id) = self
            .entries
            .iter()
            .find(|(_, entry)| {
                entry
                    .running
                    .as_ref()
                    .is_some_and(|running| running.pid == terminated.who)
            })
            .map(|(id, _)| id.clone())
        else {
            return;
        };
        let entry = self.entry(&id);
        entry.running = None;
        if entry.progress.is_owed() {
            self.reactivate(ctx, &id).await;
        } else if !entry.progress.has_caught_up() {
            // No read has yet reached the end of the journal, so this stop is
            // not a passivation: nothing counted in R says what lies past the
            // reads that failed or came back full, and the poller would have
            // read again at its next tick, which the idle stop came before.
            self.activate_after_poll_interval(ctx, &id);
        }
    }
}

impl Receive<Register> for ProjectionActivator {
    type Response = Result<(), OffsetError>;
    type Error = Infallible;

    async fn recv(
        &mut self,
        Register(declaration): Register,
        ctx: &mut ProcessContext<Self>,
    ) -> Result<Self::Response, Self::Error> {
        let id = declaration.projection_id().clone();
        if let Some(entry) = self.entries.get(&id) {
            if entry.running.is_some() {
                return Ok(Ok(()));
            }
            return Ok(self.activate(ctx, &id).await);
        }

        self.index(&id, declaration.interest());
        self.entries.insert(
            id.clone(),
            Entry {
                declaration,
                running: None,
                progress: Progress::default(),
                retrying: false,
            },
        );
        let activated = self.activate(ctx, &id).await;
        if activated.is_err() {
            if let Some(entry) = self.entries.remove(&id) {
                self.unindex(&id, entry.declaration.interest());
            }
        }
        Ok(activated)
    }
}

impl Receive<Unregister> for ProjectionActivator {
    type Response = Option<Stopped>;
    type Error = Infallible;

    async fn recv(
        &mut self,
        Unregister(id): Unregister,
        _ctx: &mut ProcessContext<Self>,
    ) -> Result<Self::Response, Self::Error> {
        let Some(entry) = self.entries.remove(&id) else {
            return Ok(None);
        };
        self.unindex(&id, entry.declaration.interest());
        let Some(running) = entry.running else {
            return Ok(None);
        };
        let (stopped, answer) = oneshot::channel();
        self.stopping.insert(running.pid, stopped);
        // A projector that has already ended refuses the request; its
        // `Terminated` is on its way all the same, and answers the stopper.
        let _ = running.projector.stop().await;
        Ok(Some(answer))
    }
}

impl Receive<IsActive> for ProjectionActivator {
    type Response = bool;
    type Error = Infallible;

    async fn recv(
        &mut self,
        IsActive(id): IsActive,
        _ctx: &mut ProcessContext<Self>,
    ) -> Result<bool, Infallible> {
        Ok(self
            .entries
            .get(&id)
            .is_some_and(|entry| entry.running.is_some()))
    }
}

impl Receive<RetryActivation> for ProjectionActivator {
    type Response = ();
    type Error = Infallible;

    async fn recv(
        &mut self,
        RetryActivation(id): RetryActivation,
        ctx: &mut ProcessContext<Self>,
    ) -> Result<(), Infallible> {
        // A projection unregistered meanwhile is not to be started; one that
        // started meanwhile has a poller reading for it.
        let Some(entry) = self.entries.get_mut(&id) else {
            return Ok(());
        };
        entry.retrying = false;
        if entry.running.is_none() {
            self.reactivate(ctx, &id).await;
        }
        Ok(())
    }
}

impl Receive<Appended> for ProjectionActivator {
    type Response = ();
    type Error = Infallible;

    async fn recv(
        &mut self,
        appended: Appended,
        ctx: &mut ProcessContext<Self>,
    ) -> Result<(), Infallible> {
        self.on_appended(ctx, &appended.key, appended.head, &appended.types)
            .await;
        Ok(())
    }
}

impl Receive<StartWatcher> for ProjectionActivator {
    type Response = ();
    type Error = Infallible;

    async fn recv(
        &mut self,
        _msg: StartWatcher,
        ctx: &mut ProcessContext<Self>,
    ) -> Result<(), Infallible> {
        let source = self.watcher().clone();
        let after = match source.position.load().await {
            Ok(position) => position.unwrap_or(0),
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    "projection activator: loading the watcher's position failed; \
                     tried again after its interval"
                );
                Self::schedule_watcher(ctx, &source);
                return Ok(());
            }
        };
        let activator = ctx.self_proxy().clone();
        DurableSubscription::new(|loaded| Some(Seen::from(loaded)))
            .with_poll_interval(source.interval)
            .spawn_batch_child_for(
                ctx,
                source.journal.clone(),
                activator,
                SequenceCursor::Global { after },
                DEFAULT_BATCH_LIMIT,
                None,
            )
            .await;
        Ok(())
    }
}

impl Receive<PolledBatch<Seen>> for ProjectionActivator {
    type Response = SettledThrough;
    type Error = Infallible;

    async fn recv(
        &mut self,
        batch: PolledBatch<Seen>,
        ctx: &mut ProcessContext<Self>,
    ) -> Result<SettledThrough, Infallible> {
        for (_observed, seen) in batch.records {
            self.on_appended(
                ctx,
                &seen.stream_key,
                seen.global_sequence,
                &[seen.event_type],
            )
            .await;
        }
        if let Err(error) = self.watcher().position.save(batch.last_observed).await {
            // Only a restart reads the saved position back, and one that
            // starts behind replays what it already reported, which wakes
            // nothing that has settled it.
            tracing::warn!(
                error = %error,
                position = batch.last_observed,
                "projection activator: saving the watcher's position failed"
            );
        }
        Ok(SettledThrough(batch.last_observed))
    }
}
