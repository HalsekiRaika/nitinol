//! The process that keeps a system's projections and wakes their projectors.

mod declaration;
mod notifier;
mod watcher;

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;

use nitinol_persistence::EventType;
use nitinol_runtime::ident::Pid;
use nitinol_runtime::process::{
    Process, ProcessContext, ProcessProxy, Receive, Terminated, TerminatedReason,
};
use nitinol_runtime::{IdleTimeout, ProcessSystem, Props, SupervisionStrategy};
use tokio::sync::oneshot;

use crate::durable_stream::{DurableSubscription, SequenceCursor};
use crate::projection::ProjectionId;

pub(crate) use self::declaration::{Declaration, ProjectorDeclaration};
pub(crate) use self::notifier::ActivatorNotifier;
pub(crate) use self::watcher::{WatcherSetting, WatcherSource};

use self::declaration::{Interest, Running};
use self::notifier::Appended;
use self::watcher::Seen;

/// The one process per system that holds every projection spawned on it, and
/// starts each one's projector whenever there is something for it to project.
///
/// - Projectors are top-level processes, not the activator's children: the
///   activator keeps the declaration each was spawned with and watches the one
///   running, so a projector may stop on its idle timeout and lose nothing by
///   it — it is built anew from the declaration the next time it is needed.
///   Activating one is spawning and watching it, nothing more; the projector
///   reads its own offset as it starts.
/// - What tells the activator a projector is needed is an append. Appends made
///   through the system's own journal are reported to it as they are accepted;
///   appends made anywhere else are found by the watcher, its child, which
///   reads the journal from its head as the first projection was registered —
///   from its start if the head could not be read — at a long interval. Each
///   projection whose stream or handlers the appended
///   events match is woken: a running projector by being told to read at once,
///   a stopped one by being activated again.
///
/// # Wake-ups are decided by position, not by time
///
/// For each projection the activator keeps two positions, in the unit the
/// projection keeps its offset in — its stream's sequence for a projection
/// following one stream, the global position for one following every stream:
/// R, the furthest event it follows that the activator has learned of, and M,
/// how far its projector has reported settling or restored its offset to.
///
/// That makes the following hold: an event the projection follows is
/// projected without waiting for another append. The activator learns of it
/// either before the projector stops — so R stays past M until a batch settles
/// it, and a projector that stops on its idle timeout owing it is activated
/// again at once — or after, when the projector is stopped and is activated
/// for it then.
///
/// What no append announced — records from before the watcher started, or
/// ones the reads of an activation failed on — is counted in neither. An
/// activation that stops on its idle timeout before it reported a settled batch
/// or a read that found nothing is activated again once its poll interval has
/// passed, which is when its poller would have read again — unless the
/// projection was activated by something else meanwhile, whose own end then
/// decides.
///
/// A stop is a system signal and is handled ahead of messages, so a report the
/// projector sent before it stopped may arrive after its `Terminated`. That
/// costs at most an activation that reads nothing and stops again.
///
/// # Restarts, stops and poisoning
///
/// The activator acts on a projector's `Terminated` and on nothing else that
/// happens to it. A supervised restart — of the poller under a projector, say
/// — is finished inside the restarted process and sends no `Terminated`, so
/// the activator never starts a second projector beside one whose child
/// restarted; a projector itself resumes on a failed batch rather than
/// restarting. A projector that was stopped other than on its idle timeout, or
/// poisoned, is not activated again for its stop: the next append it follows
/// activates it, unless it was taken out through its handle.
pub(crate) struct ProjectionActivator {
    /// Where projectors are spawned: at the top level, so that none is a child
    /// of the activator.
    system: ProcessSystem,
    entries: HashMap<ProjectionId, Entry>,
    /// Projections following every stream, under each event type a handler of
    /// theirs takes.
    by_type: HashMap<EventType, Vec<ProjectionId>>,
    /// Projections following one stream, under that stream's key.
    by_stream: HashMap<Arc<str>, Vec<ProjectionId>>,
    /// Who is waiting for which projector, stopped through its handle, to end.
    stopping: HashMap<Pid, oneshot::Sender<()>>,
    /// How many activations the activator has started across every projection.
    /// An activation is known by the number it was given, which no registration
    /// reuses, so a [`RetryActivation`] kept waiting through an unregister and
    /// a register of the same id is not mistaken for one of the new entry.
    activations: u64,
    /// The watcher not yet started: it is taken by the first registration, so
    /// a system with no projection reads nothing.
    watcher: Option<WatcherSource>,
}

/// The unit a projection keeps its offset in, and so the one its R and M are
/// kept in.
#[derive(Clone, Copy)]
enum Unit {
    StreamSequence,
    GlobalPosition,
}

impl Unit {
    fn of(interest: &Interest) -> Self {
        match interest {
            Interest::Stream(_) => Self::StreamSequence,
            Interest::EventTypes(_) => Self::GlobalPosition,
        }
    }
}

/// Where an appended event stands, in both units.
#[derive(Clone, Copy)]
struct Position {
    global: u64,
    in_stream: u64,
}

/// One registered projection.
struct Entry {
    declaration: Box<dyn Declaration>,
    unit: Unit,
    running: Option<Running>,
    /// R: the furthest event this projection follows that the activator has
    /// learned of.
    known: u64,
    /// M: how far the projection's projector has reported settling.
    settled: u64,
    /// Whether the projector of the latest activation has reported a settled
    /// batch or a read that found nothing.
    settled_once: bool,
    /// The number the activator gave this projection's latest activation: the
    /// one a [`RetryActivation`] must have been asked for to be acted on.
    activation: u64,
    /// Whether a [`ReadNow`](crate::durable_stream::ReadNow) told to the
    /// running projector awaits being taken up by it.
    ///
    /// At most one is outstanding: the activator and a projector tell each
    /// other with bounded mailboxes, and a flood of appends answered with one
    /// read each could have both wait on the other's full mailbox. It is
    /// cleared when the projector has passed the request on to its poller,
    /// whatever that read comes to, so a read that fails holds nothing back.
    read_now_pending: bool,
    /// Whether an event was learned of while one was outstanding. That event
    /// may have been appended after the poller was woken, so the request taken
    /// up does not cover it, and another is told once it is.
    read_now_owed: bool,
}

impl Entry {
    fn new(declaration: Box<dyn Declaration>, unit: Unit) -> Self {
        Self {
            declaration,
            unit,
            running: None,
            known: 0,
            settled: 0,
            settled_once: false,
            activation: 0,
            read_now_pending: false,
            read_now_owed: false,
        }
    }

    fn learn_of(&mut self, at: Position) {
        let position = match self.unit {
            Unit::StreamSequence => at.in_stream,
            Unit::GlobalPosition => at.global,
        };
        self.known = self.known.max(position);
    }

    /// Whether an event the projection follows lies past what it has settled.
    fn is_owed(&self) -> bool {
        self.known > self.settled
    }
}

/// Register a projection and have it running; a projection already registered
/// under the same id keeps its declaration, and this joins it.
pub(crate) struct Register(pub(crate) Box<dyn Declaration>);

/// Take a projection out of the registry, stopping its projector if one runs.
pub(crate) struct Unregister(pub(crate) ProjectionId);

/// Whether a projection has a projector running.
pub(crate) struct IsActive(pub(crate) ProjectionId);

/// A projection's projector settled a batch through `through`, in the unit
/// the projection keeps its offset in.
pub(crate) struct Settled {
    pub(crate) id: ProjectionId,
    pub(crate) through: u64,
}

/// A projection's projector had a read find nothing past where it stood.
pub(crate) struct CaughtUp {
    pub(crate) id: ProjectionId,
}

/// A projection's projector started from the offset it had recorded, `through`
/// in the unit the projection keeps its offset in. No read is reported by
/// this: what the offset covers was settled by an earlier activation.
pub(crate) struct OffsetRestored {
    pub(crate) id: ProjectionId,
    pub(crate) through: u64,
}

/// A projection's projector took up the [`ReadNow`](crate::durable_stream::ReadNow)
/// it was told, and passed it on to its poller.
pub(crate) struct ReadTakenUp {
    pub(crate) id: ProjectionId,
}

/// Activate a projection whose projector, the activation numbered `after`,
/// stopped before it settled anything, now that its poll interval has passed. It is
/// dropped if the projection has been activated since: what it waited for is
/// then no longer the activation that decides.
struct RetryActivation {
    id: ProjectionId,
    after: u64,
}

/// Answered once the projector [`Unregister`] stopped has ended.
pub(crate) type Stopped = oneshot::Receiver<()>;

impl ProjectionActivator {
    /// Start a system's activator on `system`, where it spawns projectors too.
    ///
    /// Called from exactly one place: the system's build, once per system. The
    /// system's journal reports its appends to the activator this returns, so
    /// they reach it from the system's first append on. It is persistent,
    /// because nothing would wake it, and resumes rather than restarting,
    /// which would drop every registration it holds.
    pub(crate) async fn spawn(
        system: &ProcessSystem,
        watcher: Option<WatcherSource>,
    ) -> ProcessProxy<Self> {
        let projectors_on = system.clone();
        let props = Props::new(move || ProjectionActivator {
            system: projectors_on.clone(),
            entries: HashMap::new(),
            by_type: HashMap::new(),
            by_stream: HashMap::new(),
            stopping: HashMap::new(),
            activations: 0,
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

    /// Start a projector for the registered projection `id`, and watch it.
    async fn activate(&mut self, ctx: &mut ProcessContext<Self>, id: &ProjectionId) {
        let activator = ctx.self_proxy().clone();
        self.activations += 1;
        let entry = self
            .entries
            .get_mut(id)
            .expect("the activator names only projections it has registered");
        let running = entry.declaration.activate(&self.system, activator).await;
        ctx.watch(running.pid).await;
        entry.running = Some(running);
        entry.activation = self.activations;
        entry.settled_once = false;
        entry.read_now_pending = false;
        entry.read_now_owed = false;
    }

    /// Activate `id` once its poll interval has passed, if its latest
    /// activation, the one that just ended, is still its latest then.
    fn activate_after_poll_interval(&mut self, ctx: &ProcessContext<Self>, id: &ProjectionId) {
        let entry = self.entry(id);
        let retry = RetryActivation {
            id: id.clone(),
            after: entry.activation,
        };
        ctx.pipe_to_self(
            tokio::time::sleep(entry.declaration.poll_interval()),
            move |_slept| retry,
        );
    }

    /// Have the running projector of `id` read at once, or once the request
    /// already told it has been taken up.
    async fn read_now(&mut self, id: &ProjectionId) {
        let entry = self.entry(id);
        let Some(running) = &entry.running else {
            return;
        };
        if entry.read_now_pending {
            entry.read_now_owed = true;
            return;
        }
        entry.read_now_pending = true;
        entry.declaration.read_now(&running.projector).await;
    }

    /// Learn that events of `types`, the last of them at `at`, were appended to
    /// the stream `key`, and wake every projection that follows them and has
    /// not settled that far.
    async fn on_appended(
        &mut self,
        ctx: &mut ProcessContext<Self>,
        key: &str,
        at: Position,
        types: &[EventType],
    ) {
        for id in self.interested_in(key, types) {
            let entry = self.entry(&id);
            entry.learn_of(at);
            if !entry.is_owed() {
                continue;
            }
            if entry.running.is_some() {
                self.read_now(&id).await;
            } else {
                self.activate(ctx, &id).await;
            }
        }
    }

    /// Start the watcher over `source` as this activator's child.
    ///
    /// Reading the journal's head here is the one read an activator handler
    /// makes itself, and it is made once in the system's lifetime.
    async fn start_watcher(&mut self, ctx: &mut ProcessContext<Self>, source: WatcherSource) {
        // Everything past the journal's head is the watcher's; what lies at or
        // before it is read by a projector's first activation. A head that
        // cannot be read leaves no such line, so the watcher reads from the
        // journal's start: it may see more than it needs to, never less.
        let after = match source.journal.head().await {
            Ok(head) => head.unwrap_or(0),
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    "projection activator: reading the journal's head for the watcher failed; \
                     the watcher reads from the journal's start"
                );
                0
            }
        };
        DurableSubscription::new(|loaded| Some(Seen::from(loaded)))
            .with_poll_interval(source.interval)
            .spawn_child(ctx, source.journal, SequenceCursor::Global { after })
            .await;
    }
}

impl Process for ProjectionActivator {
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
        match terminated.why {
            // `NotFound` is a projector that ended before it was watched, for a
            // reason not known; it is weighed as an idle stop rather than
            // risk leaving what it owed unread.
            TerminatedReason::Timeout | TerminatedReason::NotFound => {
                if entry.is_owed() {
                    self.activate(ctx, &id).await;
                } else if !entry.settled_once {
                    // Nothing counted in R says what lies past the reads that
                    // failed or were refused, and the poller would have read
                    // again at its next tick, which the idle stop came before.
                    self.activate_after_poll_interval(ctx, &id);
                }
            }
            TerminatedReason::Stopped | TerminatedReason::Poisoned => {}
        }
    }
}

impl Receive<Register> for ProjectionActivator {
    type Response = ();
    type Error = Infallible;

    async fn recv(
        &mut self,
        Register(declaration): Register,
        ctx: &mut ProcessContext<Self>,
    ) -> Result<(), Infallible> {
        // Started before the projection is activated, so the head it reads
        // lies at or before what that activation's catch-up reads.
        if let Some(source) = self.watcher.take() {
            self.start_watcher(ctx, source).await;
        }
        let id = declaration.projection_id().clone();
        if let Some(entry) = self.entries.get(&id) {
            if entry.running.is_none() {
                self.activate(ctx, &id).await;
            }
            return Ok(());
        }

        let interest = declaration.interest();
        let unit = Unit::of(&interest);
        self.index(&id, interest);
        self.entries
            .insert(id.clone(), Entry::new(declaration, unit));
        self.activate(ctx, &id).await;
        Ok(())
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
        RetryActivation { id, after }: RetryActivation,
        ctx: &mut ProcessContext<Self>,
    ) -> Result<(), Infallible> {
        // A projection unregistered meanwhile is not to be started. One
        // activated meanwhile has had its own end weighed, or has a poller
        // reading for it, so this wait is moot; with none since, the projector
        // is stopped, and it is started unless the report that it settled
        // arrived after its `Terminated`.
        let Some(entry) = self.entries.get(&id) else {
            return Ok(());
        };
        if entry.activation == after && (entry.is_owed() || !entry.settled_once) {
            self.activate(ctx, &id).await;
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
        let at = Position {
            global: appended.head,
            in_stream: appended.stream_version,
        };
        self.on_appended(ctx, &appended.key, at, &appended.types)
            .await;
        Ok(())
    }
}

impl Receive<Seen> for ProjectionActivator {
    type Response = ();
    type Error = Infallible;

    async fn recv(&mut self, seen: Seen, ctx: &mut ProcessContext<Self>) -> Result<(), Infallible> {
        let at = Position {
            global: seen.global_sequence,
            in_stream: seen.sequence,
        };
        self.on_appended(ctx, &seen.stream_key, at, &[seen.event_type])
            .await;
        Ok(())
    }
}

impl Receive<Settled> for ProjectionActivator {
    type Response = ();
    type Error = Infallible;

    async fn recv(
        &mut self,
        Settled { id, through }: Settled,
        _ctx: &mut ProcessContext<Self>,
    ) -> Result<(), Infallible> {
        // A projection unregistered meanwhile has nothing to be weighed.
        if let Some(entry) = self.entries.get_mut(&id) {
            entry.settled = entry.settled.max(through);
            entry.settled_once = true;
        }
        Ok(())
    }
}

impl Receive<CaughtUp> for ProjectionActivator {
    type Response = ();
    type Error = Infallible;

    async fn recv(
        &mut self,
        CaughtUp { id }: CaughtUp,
        _ctx: &mut ProcessContext<Self>,
    ) -> Result<(), Infallible> {
        if let Some(entry) = self.entries.get_mut(&id) {
            entry.settled_once = true;
        }
        Ok(())
    }
}

impl Receive<OffsetRestored> for ProjectionActivator {
    type Response = ();
    type Error = Infallible;

    async fn recv(
        &mut self,
        OffsetRestored { id, through }: OffsetRestored,
        _ctx: &mut ProcessContext<Self>,
    ) -> Result<(), Infallible> {
        if let Some(entry) = self.entries.get_mut(&id) {
            entry.settled = entry.settled.max(through);
        }
        Ok(())
    }
}

impl Receive<ReadTakenUp> for ProjectionActivator {
    type Response = ();
    type Error = Infallible;

    async fn recv(
        &mut self,
        ReadTakenUp { id }: ReadTakenUp,
        _ctx: &mut ProcessContext<Self>,
    ) -> Result<(), Infallible> {
        let Some(entry) = self.entries.get_mut(&id) else {
            return Ok(());
        };
        entry.read_now_pending = false;
        if std::mem::take(&mut entry.read_now_owed) {
            self.read_now(&id).await;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use async_trait::async_trait;
    use bytes::Bytes;
    use nitinol_persistence::provider::PersistenceProvider;
    use nitinol_persistence::store::{InMemoryOffsetStore, InMemoryStore, OffsetStore};
    use nitinol_persistence::{AggregateId, AppendingEvent, Family, LoadedEvent, TypeName};
    use nitinol_runtime::process::{AnyProxy, ProcessState};

    use crate::codec::Codec;
    use crate::durable_stream::{BatchPollerProcess, DEFAULT_POLL_INTERVAL};
    use crate::projection::process::ProjectorProcess;
    use crate::projection::{ProjectionContext, Projector, ProjectorHandle, ProjectorProps};
    use crate::system::EventSourceSystem;
    use crate::Event;

    #[derive(Clone)]
    struct Noted;

    struct UnitCodec;

    impl Codec<Noted> for UnitCodec {
        type Error = Infallible;

        fn encode(_event: &Noted) -> Result<Bytes, Self::Error> {
            Ok(Bytes::new())
        }

        fn decode(_payload: &[u8]) -> Result<Noted, Self::Error> {
            Ok(Noted)
        }
    }

    impl Event for Noted {
        const EVENT_TYPE: EventType = EventType::new(
            Family::new("projection.activator.test"),
            TypeName::new("Noted"),
        );

        type Codec = UnitCodec;
    }

    /// A projector that refuses every record, so whatever it is handed stays
    /// owed.
    struct Refusing {
        refused: Arc<AtomicUsize>,
    }

    #[derive(Debug, thiserror::Error)]
    #[error("the record is refused")]
    struct Refused;

    #[async_trait]
    impl Projector<Noted> for Refusing {
        type Error = Refused;

        async fn project(
            &mut self,
            _event: Noted,
            _ctx: &mut ProjectionContext<'_, ()>,
        ) -> Result<(), Self::Error> {
            self.refused.fetch_add(1, Ordering::SeqCst);
            Err(Refused)
        }
    }

    type RefusingProcess = ProjectorProcess<Refusing, InMemoryOffsetStore, ()>;

    /// How long, in virtual time, a wait below may take before it is a failure.
    const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

    /// The projector the activator has running for a projection.
    struct RunningProjector(ProjectionId);

    impl Receive<RunningProjector> for ProjectionActivator {
        type Response = Option<AnyProxy>;
        type Error = Infallible;

        async fn recv(
            &mut self,
            RunningProjector(id): RunningProjector,
            _ctx: &mut ProcessContext<Self>,
        ) -> Result<Option<AnyProxy>, Infallible> {
            Ok(self
                .entries
                .get(&id)
                .and_then(|entry| entry.running.as_ref())
                .map(|running| running.projector.clone()))
        }
    }

    /// A projector's place in the process tree: its parent, and its children.
    struct Lineage;

    impl<P, Cs, Tx> Receive<Lineage> for ProjectorProcess<P, Cs, Tx>
    where
        P: Send + Sync + 'static,
        Cs: OffsetStore<Tx = Tx> + Send + Sync + 'static,
        Tx: Send + 'static,
    {
        type Response = (Option<Pid>, Vec<Pid>);
        type Error = Infallible;

        async fn recv(
            &mut self,
            _msg: Lineage,
            ctx: &mut ProcessContext<Self>,
        ) -> Result<(Option<Pid>, Vec<Pid>), Infallible> {
            Ok((
                ctx.parent().copied(),
                ctx.children().iter().copied().collect(),
            ))
        }
    }

    /// Append the event of `stream` at `sequence` through the system's journal.
    async fn append_noted(
        system: &EventSourceSystem<InMemoryStore>,
        stream: &AggregateId,
        sequence: u64,
    ) {
        system
            .event_store()
            .append(
                stream.as_str(),
                vec![AppendingEvent {
                    sequence,
                    event_type: Noted::EVENT_TYPE,
                    payload: Bytes::new(),
                    occurred_at: jiff::Timestamp::now(),
                }],
            )
            .await
            .expect("appending to the stream must succeed");
    }

    /// Spawn the projection `name` following the stream of that name, with a
    /// projector that refuses every record and counts its production in
    /// `activations`.
    async fn spawn_refusing(
        system: &EventSourceSystem<InMemoryStore>,
        name: &str,
        idle_timeout: IdleTimeout,
        activations: &Arc<AtomicUsize>,
        refused: &Arc<AtomicUsize>,
    ) -> ProjectorHandle {
        let producer = {
            let (activations, refused) = (Arc::clone(activations), Arc::clone(refused));
            move || {
                activations.fetch_add(1, Ordering::SeqCst);
                Refusing {
                    refused: Arc::clone(&refused),
                }
            }
        };
        ProjectorProps::new(ProjectionId::new(name), producer)
            .with_event::<Noted>()
            .catchup_from_aggregate(AggregateId::new(name))
            .with_idle_timeout(idle_timeout)
            .spawn(system)
            .await
    }

    /// One projection, following one stream with a projector that refuses
    /// every record, on a system with no watcher and no default idle timeout.
    struct Fixture {
        system: EventSourceSystem<InMemoryStore>,
        stream: AggregateId,
        handle: ProjectorHandle,
        activator: ProcessProxy<ProjectionActivator>,
        projector: ProcessProxy<RefusingProcess>,
        activations: Arc<AtomicUsize>,
        refused: Arc<AtomicUsize>,
    }

    impl Fixture {
        async fn spawned(name: &str) -> Self {
            Self::spawned_with(name, IdleTimeout::Inherit, false).await
        }

        /// A fixture whose projector stops on `idle_timeout`, with the stream's
        /// first event journaled before the projection is spawned if
        /// `journaled_before_spawn` — which no notification announces.
        async fn spawned_with(
            name: &str,
            idle_timeout: IdleTimeout,
            journaled_before_spawn: bool,
        ) -> Self {
            let system = EventSourceSystem::builder(ProcessSystem::new().await)
                .with_persistence(PersistenceProvider::new(InMemoryStore::default()))
                .without_projection_watcher()
                .build()
                .await;
            let id = ProjectionId::new(name);
            let stream = AggregateId::new(name);
            if journaled_before_spawn {
                append_noted(&system, &stream, 1).await;
            }
            let activations = Arc::new(AtomicUsize::new(0));
            let refused = Arc::new(AtomicUsize::new(0));
            let handle = spawn_refusing(&system, name, idle_timeout, &activations, &refused).await;
            let activator = system.projection_activator();
            let projector = activator
                .ask(RunningProjector(id))
                .await
                .expect("the activator answers while the system runs")
                .expect("a spawned projection has a projector running")
                .downcast::<RefusingProcess>()
                .expect("the projector running is the one the props declared");
            Self {
                system,
                stream,
                handle,
                activator,
                projector,
                activations,
                refused,
            }
        }

        /// Have the projector owe an event: one announced to the activator,
        /// which the projector has been handed and refused.
        async fn owing_an_event(&self) {
            append_noted(&self.system, &self.stream, 1).await;
            let refused = async {
                while self.refused.load(Ordering::SeqCst) == 0 {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            };
            tokio::time::timeout(PROBE_TIMEOUT, refused)
                .await
                .expect("the projector must be handed the event and refuse it");
        }

        /// Wait until the projector process the fixture started has ended.
        async fn until_its_projector_ended(&self) {
            let ended = async {
                while self.projector.state() != ProcessState::Stopped {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            };
            tokio::time::timeout(PROBE_TIMEOUT, ended)
                .await
                .expect("the projector must end");
        }

        /// Wait until the projection has been activated `count` times.
        async fn until_activated(&self, count: usize) {
            let activated = async {
                while self.activations() < count {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            };
            tokio::time::timeout(PROBE_TIMEOUT, activated)
                .await
                .expect("the projection must be activated");
        }

        /// Wait until the projection has no projector running.
        async fn until_inactive(&self) {
            let inactive = async {
                while self.handle.is_active().await {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            };
            tokio::time::timeout(PROBE_TIMEOUT, inactive)
                .await
                .expect("the projector must end");
        }

        fn activations(&self) -> usize {
            self.activations.load(Ordering::SeqCst)
        }
    }

    /// A projector is spawned at the top level rather than under the activator,
    /// and reads through a poller of its own, its child.
    #[tokio::test(start_paused = true)]
    async fn a_projector_is_a_top_level_process_reading_through_its_child_poller() {
        let fixture = Fixture::spawned("projection-activator-lineage").await;

        let (parent, children) = fixture
            .projector
            .ask(Lineage)
            .await
            .expect("a running projector answers");

        assert_eq!(
            parent,
            None,
            "the projector must be top-level, not a child of the activator ({})",
            fixture.activator.pid()
        );
        let [poller] = children[..] else {
            panic!("the projector must have its poller as its one child, got {children:?}");
        };
        let poller = fixture
            .system
            .process_system()
            .lookup(poller)
            .await
            .expect("the projector's child must be running");
        assert!(
            poller
                .downcast::<BatchPollerProcess<LoadedEvent, RefusingProcess>>()
                .is_some(),
            "the projector's child must be the batch poller that reads for it"
        );
    }

    /// The projector owes an event, so a stop on its idle timeout would have
    /// it activated again at once; one stopped any other way is left stopped.
    #[tokio::test(start_paused = true)]
    async fn a_projector_stopped_owing_an_event_is_not_activated_again_for_it() {
        let fixture = Fixture::spawned("projection-activator-stopped").await;
        fixture.owing_an_event().await;

        fixture
            .projector
            .stop()
            .await
            .expect("a running projector takes a stop");
        fixture.until_its_projector_ended().await;
        tokio::time::sleep(DEFAULT_POLL_INTERVAL * 8).await;

        assert_eq!(
            fixture.activations(),
            1,
            "a projector that was stopped must not be activated again for its stop"
        );
        assert!(!fixture.handle.is_active().await);
    }

    #[tokio::test(start_paused = true)]
    async fn a_projector_poisoned_owing_an_event_is_not_activated_again_for_it() {
        let fixture = Fixture::spawned("projection-activator-poisoned").await;
        fixture.owing_an_event().await;

        fixture
            .projector
            .poison()
            .await
            .expect("a running projector takes a poison");
        fixture.until_its_projector_ended().await;
        tokio::time::sleep(DEFAULT_POLL_INTERVAL * 8).await;

        assert_eq!(
            fixture.activations(),
            1,
            "a projector that was poisoned must not be activated again for its stop"
        );
        assert!(!fixture.handle.is_active().await);
    }

    /// An activation stopped on its idle timeout before it settled anything
    /// waits out its poll interval to be activated again. An append that
    /// activates the projection first makes that wait moot: the activation it
    /// started, stopped other than on its idle timeout, is not activated again.
    #[tokio::test(start_paused = true)]
    async fn a_poll_interval_wait_does_not_outlive_the_activation_it_was_for() {
        let idle = DEFAULT_POLL_INTERVAL / 2;
        let fixture = Fixture::spawned_with(
            "projection-activator-stale-retry",
            IdleTimeout::After(idle),
            true,
        )
        .await;
        fixture.until_its_projector_ended().await;
        assert_eq!(
            fixture.activations(),
            1,
            "the retry must still be waiting out its poll interval"
        );

        append_noted(&fixture.system, &fixture.stream, 2).await;
        fixture.until_activated(2).await;
        let second = fixture
            .activator
            .ask(RunningProjector(ProjectionId::new(
                "projection-activator-stale-retry",
            )))
            .await
            .expect("the activator answers while the system runs")
            .expect("the append must have activated the projection");
        second
            .stop()
            .await
            .expect("a running projector takes a stop");
        fixture.until_inactive().await;
        tokio::time::sleep(DEFAULT_POLL_INTERVAL * 8).await;

        assert_eq!(
            fixture.activations(),
            2,
            "a wait for an earlier activation must not start a projector after a later one was stopped"
        );
        assert!(!fixture.handle.is_active().await);
    }

    /// The activation a poll interval wait was for is told apart from the
    /// ones of a projection registered anew under the same id: stopped before
    /// the wait ends, the new one is not activated by it.
    #[tokio::test(start_paused = true)]
    async fn a_poll_interval_wait_does_not_outlive_the_registration_it_was_for() {
        let name = "projection-activator-stale-retry-reregistered";
        let idle = DEFAULT_POLL_INTERVAL / 2;
        let fixture = Fixture::spawned_with(name, IdleTimeout::After(idle), true).await;
        fixture.until_its_projector_ended().await;
        assert_eq!(
            fixture.activations(),
            1,
            "the retry must still be waiting out its poll interval"
        );

        fixture.handle.stop().await;
        let handle = spawn_refusing(
            &fixture.system,
            name,
            IdleTimeout::After(idle),
            &fixture.activations,
            &fixture.refused,
        )
        .await;
        assert_eq!(
            fixture.activations(),
            2,
            "the projection is registered anew"
        );
        let second = fixture
            .activator
            .ask(RunningProjector(ProjectionId::new(name)))
            .await
            .expect("the activator answers while the system runs")
            .expect("registering the projection must have activated it");
        second
            .stop()
            .await
            .expect("a running projector takes a stop");
        let inactive = async {
            while handle.is_active().await {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        };
        tokio::time::timeout(PROBE_TIMEOUT, inactive)
            .await
            .expect("the projector must end");
        tokio::time::sleep(DEFAULT_POLL_INTERVAL * 8).await;

        assert_eq!(
            fixture.activations(),
            2,
            "a wait for a registration that was taken out must not start a projector of the new one"
        );
        assert!(!handle.is_active().await);
    }
}
