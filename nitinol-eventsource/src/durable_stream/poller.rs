use std::future::Future;
use std::marker::PhantomData;
use std::num::NonZeroUsize;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_util::future;
use futures_util::{FutureExt, StreamExt};
use nitinol_persistence::store::Journal;
use nitinol_persistence::LoadedEvent;
use nitinol_runtime::error::HandlerError;
use nitinol_runtime::process::{
    Driver, Process, ProcessContext, ProcessProxy, Receive, Terminated,
};
use nitinol_runtime::Stream;
use tokio::sync::Notify;

use crate::durable_stream::batch::{PolledBatch, SettledThrough};
use crate::durable_stream::cursor::SequenceCursor;

pub(crate) type TransformFn<T> = Arc<dyn Fn(LoadedEvent) -> Option<T> + Send + Sync + 'static>;

/// What a shared poller and the handle onto its stream both read: the transform
/// each loaded event goes through, and whether the stream has been subscribed
/// to yet.
///
/// # Why the gate is here rather than in the poller
///
/// A poller that published before anyone subscribed would advance its cursor
/// over events it dropped, so the first subscriber would read an empty stream
/// rather than the rows that were there all along. The gate closes that window:
/// the poller publishes nothing until a `subscribe` opens it, and the `Release`
/// store there is what makes the subscription visible to the `Acquire` load the
/// poller gates on.
///
/// # Why it is one cheap-clone value
///
/// The poller's declaration is rebuilt on every supervised restart, so a
/// restarted poller has to find the gate its stream's handle already opened —
/// which it does because a clone of this names the one state rather than a copy
/// of it.
pub(crate) struct SharedPollState<T> {
    inner: Arc<SharedPollStateInner<T>>,
}

struct SharedPollStateInner<T> {
    transform: TransformFn<T>,
    start_open: AtomicBool,
}

impl<T> Clone for SharedPollState<T> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<T> SharedPollState<T> {
    pub(crate) fn new(transform: TransformFn<T>) -> Self {
        Self {
            inner: Arc::new(SharedPollStateInner {
                transform,
                start_open: AtomicBool::new(false),
            }),
        }
    }

    pub(crate) fn transform(&self) -> &TransformFn<T> {
        &self.inner.transform
    }

    /// Whether the stream has been subscribed to, and so whether the poller may
    /// publish.
    pub(crate) fn is_open(&self) -> bool {
        self.inner.start_open.load(Ordering::Acquire)
    }

    /// Report the first subscription, releasing the poller to publish.
    pub(crate) fn open(&self) {
        self.inner.start_open.store(true, Ordering::Release);
    }
}

/// When a poller's next read is due, as the read before it decided.
#[derive(Debug, Clone, Copy)]
pub(crate) enum NextRead {
    /// Wait the poll interval first.
    AfterInterval,
    /// Read again at once: the last read came back full and all of it settled,
    /// so more is most likely waiting behind it.
    AtOnce,
}

pub(crate) trait Poll: Process {
    fn poll_tick(
        &mut self,
        ctx: &mut ProcessContext<Self>,
    ) -> impl Future<Output = Result<NextRead, HandlerError>> + Send
    where
        Self: Sized;
}

/// A hint that what a poller reads has grown, so its next read need not wait
/// out the interval.
///
/// A nudge is a hint and nothing more: the poller still reads from its own
/// cursor, so a nudge that arrives early, twice, or not at all changes when a
/// record is read and never whether or how often it is delivered. Nudges given
/// while the poller is reading are folded into one, which has it read once
/// more as soon as that read is done. A `notified` future dropped by the
/// lifecycle loop after it took the nudge hands it on to the next one, so the
/// read is not lost to a system signal either.
#[derive(Clone, Default)]
pub(crate) struct Nudge(Arc<Notify>);

impl Nudge {
    /// Have the poller this was handed to read without waiting out its
    /// interval.
    pub(crate) fn nudge(&self) {
        self.0.notify_one();
    }

    async fn notified(&self) {
        self.0.notified().await;
    }
}

/// What a batch poller tells the process that owns it about its reads.
///
/// The owner learns of a read from the poller itself rather than from the
/// subscriber's answer, because the subscriber may have stopped before it was
/// handed what the poller read.
pub(crate) trait ReadReport: Send + Sync {
    /// A read returned records, the last of them at global position `through`,
    /// which the poller is about to hand to the subscriber. Called before the
    /// hand-over, so the records are accounted for even if the subscriber is
    /// gone by then.
    fn handing_over(&self, through: u64);

    /// A read ran to the end of the journal, came back short of the batch
    /// limit, and what it returned has been dealt with: nothing, or a batch the
    /// subscriber answered. Reported again by every such read.
    fn caught_up(&self);
}

/// What the process that owns a batch poller and the poller tell each other:
/// the owner nudges it, and it reports its reads.
#[derive(Clone)]
pub(crate) struct PollerLink {
    pub(crate) nudge: Nudge,
    pub(crate) reads: Arc<dyn ReadReport>,
}

/// Ticks a poller once as soon as it starts, then once every `interval`.
///
/// The interval is the wait between consecutive reads, so a poller started
/// over a backlog reads it at once rather than an interval later. A tick that
/// answers [`NextRead::AtOnce`] has the next one follow without the wait. The
/// driver lives as long as its process: a supervised restart rebuilds the
/// poller's state but not this, so a restarted poller waits the interval before
/// its first read.
///
/// Skipping the wait still hands each read back to the lifecycle loop, which
/// looks for a system signal before it asks for the next tick, so a stop
/// waits for at most the read in progress rather than for a whole backlog.
///
/// A driver handed a [`Nudge`] also ends the wait as soon as it is nudged.
pub(crate) struct IntervalDriver<P> {
    interval: Duration,
    nudge: Option<Nudge>,
    has_ticked: bool,
    read_at_once: bool,
    _phantom: PhantomData<fn() -> P>,
}

impl<P> IntervalDriver<P> {
    pub(crate) fn new(interval: Duration, nudge: Option<Nudge>) -> Self {
        Self {
            interval,
            nudge,
            has_ticked: false,
            read_at_once: false,
            _phantom: PhantomData,
        }
    }

    async fn wait_interval(&self) {
        let interval = tokio::time::sleep(self.interval);
        match &self.nudge {
            Some(nudge) => {
                let nudged = nudge.notified();
                future::select(std::pin::pin!(interval), std::pin::pin!(nudged)).await;
            }
            None => interval.await,
        }
    }
}

impl<P> Driver<P> for IntervalDriver<P>
where
    P: Poll,
{
    type Event = ();

    async fn next(&mut self) -> Option<Self::Event> {
        // The flags are read and set inside the future rather than when it is
        // made: the lifecycle loop drops a `next` future it never polled when
        // a system signal is waiting, and flags set at creation would lose the
        // first tick, or the read owed at once, to that.
        if self.has_ticked && !self.read_at_once {
            self.wait_interval().await;
        }
        self.has_ticked = true;
        self.read_at_once = false;
        Some(())
    }

    async fn apply(
        &mut self,
        state: &mut P,
        ctx: &mut ProcessContext<P>,
        _ev: (),
    ) -> Result<(), HandlerError> {
        let outcome = AssertUnwindSafe(state.poll_tick(ctx))
            .catch_unwind()
            .await
            .unwrap_or_else(|_panic| {
                tracing::warn!("durable_stream: poller panic caught — supervisor will restart");
                Err(HandlerError)
            });
        self.read_at_once = matches!(outcome, Ok(NextRead::AtOnce));
        outcome.map(|_| ())
    }

    fn supports_idle_timeout(&self) -> bool {
        false
    }
}

pub(crate) struct DurablePollerProcess<T> {
    pub(crate) store: Journal,
    pub(crate) publisher: ProcessProxy<Stream<T>>,
    pub(crate) shared: SharedPollState<T>,
    pub(crate) cursor: SequenceCursor,
}

impl<T> Process for DurablePollerProcess<T> where T: 'static + Send + Sync + Clone {}

impl<T> Poll for DurablePollerProcess<T>
where
    T: 'static + Send + Sync + Clone,
{
    async fn poll_tick(
        &mut self,
        _ctx: &mut ProcessContext<Self>,
    ) -> Result<NextRead, HandlerError> {
        if !self.shared.is_open() {
            return Ok(NextRead::AfterInterval);
        }
        poll_once(
            &self.store,
            &self.publisher,
            self.shared.transform(),
            &mut self.cursor,
        )
        .await?;
        Ok(NextRead::AfterInterval)
    }
}

/// Have a direct poller outlive its subscriber by no more than it takes to see
/// it stop.
async fn watch_subscriber<Pl, S>(ctx: &mut ProcessContext<Pl>, subscriber: &ProcessProxy<S>)
where
    Pl: Process,
    S: Process,
{
    // When this poller is spawned as a child of the subscriber (the common
    // case via `DurableSubscription::spawn_child`), the parent/child
    // hierarchy already guarantees the poller stops when the subscriber
    // stops — registering an explicit DeathWatch here would cause each
    // subscriber restart to accumulate a stale poller PID in the
    // subscriber's watcher set.
    //
    // Only register the watch when the poller was spawned as a top-level
    // process (subscriber is NOT the parent) so that it still self-stops
    // when the subscriber terminates.
    if ctx.parent() != Some(&subscriber.pid()) {
        ctx.watch(subscriber.pid()).await;
    }
}

async fn stop_with_subscriber<Pl, S>(
    terminated: Terminated,
    subscriber: &ProcessProxy<S>,
    ctx: &mut ProcessContext<Pl>,
) where
    Pl: Process,
    S: Process,
{
    if terminated.who == subscriber.pid() {
        let _ = ctx.stop_self().await;
    }
}

pub(crate) struct DirectPollerProcess<T, P> {
    pub(crate) store: Journal,
    pub(crate) subscriber: ProcessProxy<P>,
    pub(crate) transform: TransformFn<T>,
    pub(crate) cursor: SequenceCursor,
}

impl<T, P> Process for DirectPollerProcess<T, P>
where
    T: 'static + Send + Sync,
    P: Process + Receive<T, Response = ()>,
{
    async fn on_start(&mut self, ctx: &mut ProcessContext<Self>) {
        watch_subscriber(ctx, &self.subscriber).await;
    }

    async fn on_terminated(&mut self, terminated: Terminated, ctx: &mut ProcessContext<Self>) {
        stop_with_subscriber(terminated, &self.subscriber, ctx).await;
    }
}

impl<T, P> Poll for DirectPollerProcess<T, P>
where
    T: 'static + Send + Sync,
    P: Process + Receive<T, Response = ()>,
{
    async fn poll_tick(
        &mut self,
        _ctx: &mut ProcessContext<Self>,
    ) -> Result<NextRead, HandlerError> {
        poll_direct_once(
            &self.store,
            &self.subscriber,
            &self.transform,
            &mut self.cursor,
        )
        .await?;
        Ok(NextRead::AfterInterval)
    }
}

/// A direct poller that hands its subscriber what each read returned as one
/// [`PolledBatch`], rather than a record at a time.
///
/// Each read asks the journal for at most `batch_limit` records. The cursor
/// moves to the position the subscriber answers it settled through — the
/// whole batch, or the part of it before a record it could not settle — and
/// stays put when it settled nothing.
pub(crate) struct BatchPollerProcess<T, P> {
    pub(crate) store: Journal,
    pub(crate) subscriber: ProcessProxy<P>,
    pub(crate) transform: TransformFn<T>,
    pub(crate) cursor: SequenceCursor,
    pub(crate) batch_limit: NonZeroUsize,
    /// Told of each read.
    pub(crate) reads: Option<Arc<dyn ReadReport>>,
}

impl<T, P> Process for BatchPollerProcess<T, P>
where
    T: 'static + Send + Sync,
    P: Process + Receive<PolledBatch<T>, Response = SettledThrough>,
{
    async fn on_start(&mut self, ctx: &mut ProcessContext<Self>) {
        watch_subscriber(ctx, &self.subscriber).await;
    }

    async fn on_terminated(&mut self, terminated: Terminated, ctx: &mut ProcessContext<Self>) {
        stop_with_subscriber(terminated, &self.subscriber, ctx).await;
    }
}

impl<T, P> Poll for BatchPollerProcess<T, P>
where
    T: 'static + Send + Sync,
    P: Process + Receive<PolledBatch<T>, Response = SettledThrough>,
{
    async fn poll_tick(
        &mut self,
        _ctx: &mut ProcessContext<Self>,
    ) -> Result<NextRead, HandlerError> {
        Ok(poll_batch_once(
            &self.store,
            &self.subscriber,
            &self.transform,
            &mut self.cursor,
            self.batch_limit,
            self.reads.as_deref(),
        )
        .await)
    }
}

/// Read at most `batch_limit` records past the cursor, hand them to `proxy` as
/// one batch, and move the cursor to where its answer says they settled.
///
/// Only a read that came back with `batch_limit` records or more, read to its
/// end and settled whole, asks for the next read at once: anything short of
/// that has nothing more waiting, or something that needs the interval before
/// it is tried again.
///
/// `reads` is told, before a batch is handed to `proxy`, how far the records in
/// it reach, and afterwards, when a read ran to its end, came back short of
/// `batch_limit`, and either returned nothing for `proxy` or had its batch
/// answered by it, that the poller is caught up.
async fn poll_batch_once<T, P>(
    store: &Journal,
    proxy: &ProcessProxy<P>,
    transform: &TransformFn<T>,
    cursor: &mut SequenceCursor,
    batch_limit: NonZeroUsize,
    reads: Option<&dyn ReadReport>,
) -> NextRead
where
    T: 'static + Send + Sync,
    P: Process + Receive<PolledBatch<T>, Response = SettledThrough>,
{
    let query = cursor.to_load_query().with_limit(batch_limit.get());
    let stream = match store.load(query).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = ?e, "durable_stream: subscriber batch poll load failed");
            return NextRead::AfterInterval;
        }
    };

    let mut records = Vec::new();
    let mut read = 0usize;
    let mut last_observed = None;
    let mut highest_global = 0;
    let mut read_to_end = true;
    futures_util::pin_mut!(stream);
    while let Some(item) = stream.next().await {
        let loaded = match item {
            Ok(ev) => ev,
            Err(e) => {
                // What was read before the error is still delivered: it is a
                // prefix of the journal past the cursor, so settling it is
                // sound, and the rest is read again from where it ends.
                tracing::warn!(error = ?e, "durable_stream: subscriber batch stream error");
                read_to_end = false;
                break;
            }
        };
        read += 1;
        let observed = cursor.observed(&loaded);
        last_observed = Some(observed);
        let global = loaded.global_sequence;
        if let Some(value) = transform(loaded) {
            highest_global = highest_global.max(global);
            records.push((observed, value));
        }
    }

    let reached_end = read_to_end && read < batch_limit.get();

    let Some(last_observed) = last_observed else {
        if let (true, Some(reads)) = (reached_end, reads) {
            reads.caught_up();
        }
        return NextRead::AfterInterval;
    };

    let settled_through = if records.is_empty() {
        Some(last_observed)
    } else {
        // Before the hand-over, not after: the subscriber may have stopped by
        // the time it is asked, and what was read is then owed to nobody unless
        // the owner has been told of it already.
        if let Some(reads) = reads {
            reads.handing_over(highest_global);
        }
        match proxy
            .ask(PolledBatch {
                records,
                last_observed,
            })
            .await
        {
            Ok(SettledThrough(through)) => Some(through),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "durable_stream: subscriber batch ask failed; \
                     cursor not advanced — upstream batch not treated as processed"
                );
                None
            }
        }
    };
    if let Some(through) = settled_through {
        cursor.advance(through);
        // Not before the records were handed over and answered: a read that
        // reached the end but whose batch the subscriber never took is not a
        // finished catch-up.
        if let (true, Some(reads)) = (reached_end, reads) {
            reads.caught_up();
        }
    }
    let settled_whole = settled_through == Some(last_observed);

    if read_to_end && settled_whole && read >= batch_limit.get() {
        NextRead::AtOnce
    } else {
        NextRead::AfterInterval
    }
}

async fn poll_direct_once<T, P>(
    store: &Journal,
    proxy: &ProcessProxy<P>,
    transform: &TransformFn<T>,
    cursor: &mut SequenceCursor,
) -> Result<(), HandlerError>
where
    T: 'static + Send + Sync,
    P: Process + Receive<T, Response = ()>,
{
    let query = cursor.to_load_query();
    let stream = match store.load(query).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = ?e, "durable_stream: subscriber direct poll load failed");
            return Ok(());
        }
    };

    futures_util::pin_mut!(stream);
    while let Some(item) = stream.next().await {
        let loaded = match item {
            Ok(ev) => ev,
            Err(e) => {
                tracing::warn!(error = ?e, "durable_stream: subscriber direct stream error");
                return Ok(());
            }
        };

        let observed = cursor.observed(&loaded);
        match transform(loaded) {
            Some(value) => {
                // Use ask() so the cursor only advances when the subscriber's
                // handler completes successfully.  When the handler returns Err
                // (e.g. a saga whose own append the store refused), ask()
                // propagates that error and the cursor stays at the same
                // position — the upstream message is NOT treated as processed
                // (state consistency).
                match proxy.ask(value).await {
                    Ok(_) => cursor.advance(observed),
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "durable_stream: subscriber direct ask failed; \
                             cursor not advanced — upstream message not treated as processed"
                        );
                        return Ok(());
                    }
                }
            }
            None => cursor.advance(observed),
        }
    }
    Ok(())
}

async fn poll_once<T>(
    store: &Journal,
    stream_proxy: &ProcessProxy<Stream<T>>,
    transform: &TransformFn<T>,
    cursor: &mut SequenceCursor,
) -> Result<(), HandlerError>
where
    T: 'static + Send + Sync + Clone,
{
    let query = cursor.to_load_query();
    let stream = match store.load(query).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = ?e, "durable_stream: event store load failed");
            return Ok(());
        }
    };

    futures_util::pin_mut!(stream);
    while let Some(item) = stream.next().await {
        let loaded = match item {
            Ok(ev) => ev,
            Err(e) => {
                tracing::warn!(error = ?e, "durable_stream: event store stream error");
                return Ok(());
            }
        };

        let observed = cursor.observed(&loaded);
        match transform(loaded) {
            Some(value) => match stream_proxy.publish(value).await {
                Ok(()) => cursor.advance(observed),
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "durable_stream: publish failed, will retry on next poll"
                    );
                    return Ok(());
                }
            },
            None => cursor.advance(observed),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Mutex;

    use bytes::Bytes;
    use nitinol_persistence::store::InMemoryEventStore;
    use nitinol_persistence::{AppendingEvent, EventType, Family, TypeName};
    use nitinol_runtime::{ProcessSystem, Props};

    /// A batch subscriber that takes every batch whole, or refuses every one.
    struct Subscriber {
        takes: bool,
    }

    impl Process for Subscriber {}

    impl Receive<PolledBatch<u8>> for Subscriber {
        type Response = SettledThrough;
        type Error = std::io::Error;

        async fn recv(
            &mut self,
            batch: PolledBatch<u8>,
            _ctx: &mut ProcessContext<Self>,
        ) -> Result<SettledThrough, std::io::Error> {
            if self.takes {
                Ok(SettledThrough(batch.last_observed))
            } else {
                Err(std::io::Error::other("the batch is refused"))
            }
        }
    }

    /// What a poller reported of its reads, in order.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Reported {
        HandingOver(u64),
        CaughtUp,
    }

    #[derive(Default)]
    struct Reports(Mutex<Vec<Reported>>);

    impl Reports {
        fn all(&self) -> Vec<Reported> {
            self.0.lock().expect("no report panics").clone()
        }

        fn push(&self, reported: Reported) {
            self.0.lock().expect("no report panics").push(reported);
        }
    }

    impl ReadReport for Reports {
        fn handing_over(&self, through: u64) {
            self.push(Reported::HandingOver(through));
        }

        fn caught_up(&self) {
            self.push(Reported::CaughtUp);
        }
    }

    async fn journal_of(events: u64) -> Journal {
        let journal = Journal::from(InMemoryEventStore::default());
        for sequence in 1..=events {
            journal
                .append(
                    "poller-reports",
                    vec![AppendingEvent {
                        sequence,
                        event_type: EventType::new(
                            Family::new("poller.test"),
                            TypeName::new("Appended"),
                        ),
                        payload: Bytes::new(),
                        occurred_at: jiff::Timestamp::now(),
                    }],
                )
                .await
                .expect("appending the next event must succeed");
        }
        journal
    }

    /// Poll once, with a batch limit of 10, over a journal of `events` events,
    /// handing them to a subscriber that `takes` the batch or refuses it, and
    /// answer the cursor and what the poller reported.
    async fn poll_once_over(events: u64, takes: bool) -> (SequenceCursor, Vec<Reported>) {
        let journal = journal_of(events).await;
        let subscriber = ProcessSystem::new()
            .await
            .spawn(Props::new(move || Subscriber { takes }))
            .await;
        let transform: TransformFn<u8> = Arc::new(|_loaded| Some(0));
        let mut cursor = SequenceCursor::Global { after: 0 };
        let reports = Reports::default();
        poll_batch_once(
            &journal,
            &subscriber,
            &transform,
            &mut cursor,
            NonZeroUsize::new(10).expect("10 is not zero"),
            Some(&reports),
        )
        .await;
        (cursor, reports.all())
    }

    /// A read that ran to the end of the journal says nothing of what its
    /// batch became until the subscriber has answered: a subscriber that has
    /// stopped, or refuses the batch, leaves records unprojected. They are
    /// reported as owed, though, before the subscriber is asked.
    #[tokio::test]
    async fn a_batch_the_subscriber_refuses_is_reported_as_owed_and_not_as_caught_up() {
        let (cursor, reported) = poll_once_over(2, false).await;

        assert_eq!(
            reported,
            [Reported::HandingOver(2)],
            "the records must be reported through the last one, and not as read to the end"
        );
        assert!(
            matches!(cursor, SequenceCursor::Global { after: 0 }),
            "the cursor must stay behind the batch the subscriber refused"
        );
    }

    #[tokio::test]
    async fn a_batch_the_subscriber_takes_is_reported_as_owed_and_then_as_caught_up() {
        let (cursor, reported) = poll_once_over(2, true).await;

        assert_eq!(
            reported,
            [Reported::HandingOver(2), Reported::CaughtUp],
            "the records must be reported before the answer, and the end after it"
        );
        assert!(
            matches!(cursor, SequenceCursor::Global { after: 2 }),
            "the cursor must move to what the subscriber settled"
        );
    }

    #[tokio::test]
    async fn a_read_of_nothing_is_reported_as_caught_up_without_any_record() {
        let (_cursor, reported) = poll_once_over(0, true).await;

        assert_eq!(reported, [Reported::CaughtUp]);
    }
}
