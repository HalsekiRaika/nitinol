use std::convert::Infallible;
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
use nitinol_runtime::error::{AskError, HandlerError};
use nitinol_runtime::process::{
    Driver, Process, ProcessContext, ProcessProxy, ProcessWaker, Receive, Terminated,
};
use nitinol_runtime::Stream;

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

/// Have a batch poller read now rather than once its interval has passed.
///
/// A hint and nothing more: the poller still reads from its own cursor, so a
/// `ReadNow` that arrives early, twice, or not at all changes when a record is
/// read and never whether or how often it is delivered.
pub(crate) struct ReadNow;

/// Told by a batch poller to its subscriber the first time a read of its finds
/// nothing past the cursor at the end of the journal.
///
/// Once per poller incarnation, not once per such read: every message a
/// subscriber handles restarts its idle timer, so a subscriber told of each
/// empty read would never stop idle.
pub(crate) struct ReachedEnd;

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
/// The driver honours its process's waker: a wake ends the wait at once, so
/// the poller reads without waiting out the interval. Wakes given while the
/// poller is reading are kept and folded into one, which has it read once more
/// as soon as that read is done.
pub(crate) struct IntervalDriver<P> {
    interval: Duration,
    /// Handed over by the lifecycle before the process starts.
    waker: Option<ProcessWaker>,
    has_ticked: bool,
    read_at_once: bool,
    _phantom: PhantomData<fn() -> P>,
}

impl<P> IntervalDriver<P> {
    pub(crate) fn new(interval: Duration) -> Self {
        Self {
            interval,
            waker: None,
            has_ticked: false,
            read_at_once: false,
            _phantom: PhantomData,
        }
    }

    async fn wait_interval(&self) {
        let interval = tokio::time::sleep(self.interval);
        match &self.waker {
            Some(waker) => {
                let woken = waker.woken();
                future::select(std::pin::pin!(interval), std::pin::pin!(woken)).await;
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

    fn attach_waker(&mut self, waker: ProcessWaker) {
        self.waker = Some(waker);
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
///
/// The subscriber is told [`ReachedEnd`] the first time a read finds nothing
/// past the cursor, and a [`ReadNow`] has the poller read at once.
pub(crate) struct BatchPollerProcess<T, P> {
    pub(crate) store: Journal,
    pub(crate) subscriber: ProcessProxy<P>,
    pub(crate) transform: TransformFn<T>,
    pub(crate) cursor: SequenceCursor,
    pub(crate) batch_limit: NonZeroUsize,
    /// Whether this incarnation has told the subscriber [`ReachedEnd`].
    pub(crate) reported_end: bool,
}

impl<T, P> Process for BatchPollerProcess<T, P>
where
    T: 'static + Send + Sync,
    P: Process
        + Receive<PolledBatch<T>, Response = SettledThrough>
        + Receive<ReachedEnd, Response = ()>,
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
    P: Process
        + Receive<PolledBatch<T>, Response = SettledThrough>
        + Receive<ReachedEnd, Response = ()>,
{
    async fn poll_tick(
        &mut self,
        _ctx: &mut ProcessContext<Self>,
    ) -> Result<NextRead, HandlerError> {
        let read = poll_batch_once(
            &self.store,
            &self.subscriber,
            &self.transform,
            &mut self.cursor,
            self.batch_limit,
        )
        .await;
        if read.found_nothing && !self.reported_end {
            self.reported_end = true;
            if let Err(error) = self.subscriber.tell(ReachedEnd).await {
                tracing::debug!(
                    error = %error,
                    "durable_stream: the subscriber has stopped; the end of the journal is not \
                     reported to it"
                );
            }
        }
        Ok(read.next)
    }
}

impl<T, P> Receive<ReadNow> for BatchPollerProcess<T, P>
where
    T: 'static + Send + Sync,
    P: Process
        + Receive<PolledBatch<T>, Response = SettledThrough>
        + Receive<ReachedEnd, Response = ()>,
{
    type Response = ();
    type Error = Infallible;

    async fn recv(
        &mut self,
        _msg: ReadNow,
        ctx: &mut ProcessContext<Self>,
    ) -> Result<(), Infallible> {
        ctx.waker().wake();
        Ok(())
    }
}

/// What one read of a batch poller came to.
struct BatchRead {
    next: NextRead,
    /// The read ran to the end of the journal and found nothing past the
    /// cursor.
    found_nothing: bool,
}

/// Read at most `batch_limit` records past the cursor, hand them to `proxy` as
/// one batch, and move the cursor to where its answer says they settled.
///
/// Only a read that came back with `batch_limit` records or more, read to its
/// end and settled whole, asks for the next read at once: anything short of
/// that has nothing more waiting, or something that needs the interval before
/// it is tried again.
async fn poll_batch_once<T, P>(
    store: &Journal,
    proxy: &ProcessProxy<P>,
    transform: &TransformFn<T>,
    cursor: &mut SequenceCursor,
    batch_limit: NonZeroUsize,
) -> BatchRead
where
    T: 'static + Send + Sync,
    P: Process + Receive<PolledBatch<T>, Response = SettledThrough>,
{
    let after_interval = BatchRead {
        next: NextRead::AfterInterval,
        found_nothing: false,
    };
    let query = cursor.to_load_query().with_limit(batch_limit.get());
    let stream = match store.load(query).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = ?e, "durable_stream: subscriber batch poll load failed");
            return after_interval;
        }
    };

    let mut records = Vec::new();
    let mut read = 0usize;
    let mut last_observed = None;
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
        if let Some(value) = transform(loaded) {
            records.push((observed, value));
        }
    }

    let Some(last_observed) = last_observed else {
        return BatchRead {
            next: NextRead::AfterInterval,
            found_nothing: read_to_end,
        };
    };

    let settled_through = if records.is_empty() {
        Some(last_observed)
    } else {
        match proxy
            .ask(PolledBatch {
                records,
                last_observed,
            })
            .await
        {
            Ok(SettledThrough(through)) => Some(through),
            // A subscriber that stops — on its idle timeout, say — while a batch
            // waits in its mailbox drops it unanswered, and stops this poller
            // next; that is no failure of the batch.
            Err(e @ (AskError::ReplyDropped | AskError::DeadLetter { .. })) => {
                tracing::debug!(
                    error = %e,
                    "durable_stream: the subscriber stopped before answering the batch; \
                     cursor not advanced"
                );
                None
            }
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
    }
    let settled_whole = settled_through == Some(last_observed);

    if read_to_end && settled_whole && read >= batch_limit.get() {
        BatchRead {
            next: NextRead::AtOnce,
            found_nothing: false,
        }
    } else {
        after_interval
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

    use std::sync::atomic::AtomicUsize;

    use bytes::Bytes;
    use nitinol_persistence::store::InMemoryEventStore;
    use nitinol_persistence::{AppendingEvent, EventType, Family, TypeName};
    use nitinol_runtime::{ProcessSystem, Props};

    use crate::durable_stream::DurableSubscription;

    /// A batch subscriber that takes every batch whole, or refuses every one,
    /// and counts the times it was told the end of the journal was reached.
    ///
    /// Started with a `journal`, it reads it through a batch poller of its own,
    /// as a projector does.
    struct Subscriber {
        takes: bool,
        ends: Arc<AtomicUsize>,
        reads: Option<(Journal, Duration)>,
    }

    impl Subscriber {
        fn taking(takes: bool) -> Self {
            Self {
                takes,
                ends: Arc::default(),
                reads: None,
            }
        }
    }

    impl Process for Subscriber {
        async fn on_start(&mut self, ctx: &mut ProcessContext<Self>) {
            if let Some((journal, interval)) = self.reads.clone() {
                DurableSubscription::new(|_loaded| Some(0u8))
                    .with_poll_interval(interval)
                    .spawn_batch_child(
                        ctx,
                        journal,
                        SequenceCursor::Global { after: 0 },
                        NonZeroUsize::new(10).expect("10 is not zero"),
                    )
                    .await;
            }
        }
    }

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

    impl Receive<ReachedEnd> for Subscriber {
        type Response = ();
        type Error = Infallible;

        async fn recv(
            &mut self,
            _msg: ReachedEnd,
            _ctx: &mut ProcessContext<Self>,
        ) -> Result<(), Infallible> {
            self.ends.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    async fn journal_of(events: u64) -> Journal {
        let journal = Journal::from(InMemoryEventStore::default());
        for sequence in 1..=events {
            journal
                .append(
                    "poller-reads",
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
    /// answer the cursor and what the read came to.
    async fn poll_once_over(events: u64, takes: bool) -> (SequenceCursor, BatchRead) {
        let journal = journal_of(events).await;
        let subscriber = ProcessSystem::new()
            .await
            .spawn(Props::new(move || Subscriber::taking(takes)))
            .await;
        let transform: TransformFn<u8> = Arc::new(|_loaded| Some(0));
        let mut cursor = SequenceCursor::Global { after: 0 };
        let read = poll_batch_once(
            &journal,
            &subscriber,
            &transform,
            &mut cursor,
            NonZeroUsize::new(10).expect("10 is not zero"),
        )
        .await;
        (cursor, read)
    }

    #[tokio::test]
    async fn a_batch_the_subscriber_refuses_leaves_the_cursor_behind_it() {
        let (cursor, read) = poll_once_over(2, false).await;

        assert!(
            matches!(cursor, SequenceCursor::Global { after: 0 }),
            "the cursor must stay behind the batch the subscriber refused"
        );
        assert!(
            !read.found_nothing,
            "a read that returned records did not find nothing"
        );
    }

    #[tokio::test]
    async fn a_batch_the_subscriber_takes_moves_the_cursor_to_what_it_settled() {
        let (cursor, read) = poll_once_over(2, true).await;

        assert!(
            matches!(cursor, SequenceCursor::Global { after: 2 }),
            "the cursor must move to what the subscriber settled"
        );
        assert!(
            !read.found_nothing,
            "a read that returned records did not find nothing"
        );
    }

    #[tokio::test]
    async fn a_read_past_the_end_of_the_journal_finds_nothing() {
        let (_cursor, read) = poll_once_over(0, true).await;

        assert!(read.found_nothing);
    }

    /// Every read of an empty journal finds nothing, and the subscriber is
    /// told so once: each telling would restart its idle timer.
    #[tokio::test(start_paused = true)]
    async fn the_end_of_the_journal_is_reported_to_the_subscriber_once_per_poller() {
        let interval = Duration::from_millis(10);
        let ends = Arc::new(AtomicUsize::new(0));
        let journal = journal_of(0).await;
        let subscriber_ends = Arc::clone(&ends);
        ProcessSystem::new()
            .await
            .spawn(Props::new(move || Subscriber {
                takes: true,
                ends: Arc::clone(&subscriber_ends),
                reads: Some((journal.clone(), interval)),
            }))
            .await;

        tokio::time::sleep(interval * 10).await;

        assert_eq!(
            ends.load(Ordering::SeqCst),
            1,
            "ten reads of an empty journal must tell the subscriber of the end once"
        );
    }
}
