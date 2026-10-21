//! How often a saga asks an observed source is set where that source is bound.
//!
//! ```gherkin
//!   Scenario: a deployment chooses how often one source is polled
//!     Given a saga observing a source whose subscription names a poll interval
//!     When it runs for a while with nothing upstream to observe
//!     Then the gap between that source's queries is at least that interval
//!     And a source bound with a shorter interval queries more often in the
//!         same window
//!
//!   Scenario: a source that names no interval keeps the cadence it has today
//!     Given one source bound with no interval named and one bound with a long
//!           one
//!     When both run for the same window
//!     Then the unnamed one's gaps stay well inside the long interval while the
//!          named one's stay at it — an interval reaches the source it was
//!          named on and no other, and naming none is not a way of naming
//!          something
//! ```
//!
//! `Subscription::with_poll_interval` is the observing side's counterpart of
//! `SagaDeliveryProps::with_poll_interval`, and the two are observed the same
//! way: `saga_delivery_poll_interval.rs` pins the delivery host's, this file
//! pins a declared source's.
//!
//! All three clauses are needed. The lower bound alone would pass a wiring that
//! ignored the value and polled slowly for some other reason; the contrast
//! alone would pass one that treated the value as a vague hint; and without the
//! third, a wiring that quietly applied a configured interval to *every* source
//! — or one that read an unnamed interval as "do not poll" — would go
//! unnoticed. Together they say the value named is the value used, and that
//! naming nothing is not a way of naming something.
//!
//! The exact cadence of an unnamed source is `nitinol-eventsource`'s own
//! default and is deliberately not restated here: this file pins that the saga
//! side keeps using it, not what it is.
//!
//! The fixture saga's `correlate` answers `None`, which is what the delivery
//! fixtures declare it as. That is immaterial to every clause here: the subject
//! is the cadence of a source's poller, which is upstream of correlation — and
//! an upstream stream nothing is ever appended to leaves every recorded load a
//! poll rather than work.
//!
//! Run with:
//! `cargo test -p nitinol-saga --test saga_subscription_poll_interval`.

#[path = "common/delivery.rs"]
mod delivery;
use delivery::{DeliverySaga, Triggered, TEST_POLL_INTERVAL};

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use nitinol_eventsource::system::EventSourceSystem;
use nitinol_persistence::error::{AppendError, LoadError};
use nitinol_persistence::provider::{CustomStore, PersistenceProvider, Unprovided};
use nitinol_persistence::store::{
    EventStore, EventStream, InMemoryEventStore, InMemoryOffsetStore,
};
use nitinol_persistence::{AggregateId, AppendOutcome, AppendingEvent, LoadQuery};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{SagaHostProxy, SagaManagerProps, Subscription};

/// Deliberately far from any framework default, and far enough above it that a
/// source polling at a default would be caught by the counts below.
const SLOW: Duration = Duration::from_millis(500);

/// Long enough for several slow polls, so the counts are about the interval
/// rather than about startup.
const WINDOW: Duration = Duration::from_millis(1_800);

/// The upstream stream every source below is bound to.
///
/// Nothing is ever appended to it: the subject is how often the source asks,
/// so an empty stream keeps every recorded load a poll and not work.
const UPSTREAM_KEY: &str = "saga-subscription-poll-interval-upstream";

/// An `EventStore` that records when the observed source's stream was asked
/// for.
///
/// A system distributes one journal, so the source's poller and the delivery
/// host the same spawn starts are tenants of one store. The two are kept apart
/// by what they ask for: a source names the stream key it is bound to, a
/// delivery host names its saga's decision type and no key at all — so
/// recording only the calls that name [`UPSTREAM_KEY`] leaves the gaps below
/// the source's cadence rather than the host's.
#[derive(Clone)]
struct SourcePollRecorder {
    inner: InMemoryEventStore,
    /// Shared behind the handle, so the store named in the wiring and the
    /// handle a clause reads are one region.
    polls: Arc<Mutex<Vec<Instant>>>,
}

impl SourcePollRecorder {
    fn new() -> Self {
        Self {
            inner: InMemoryEventStore::default(),
            polls: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// The gaps between consecutive polls of the observed source, in call
    /// order.
    fn load_gaps(&self) -> Vec<Duration> {
        self.polls
            .lock()
            .expect("recorded polls must not be poisoned")
            .windows(2)
            .map(|pair| pair[1].saturating_duration_since(pair[0]))
            .collect()
    }

    fn load_count(&self) -> usize {
        self.polls
            .lock()
            .expect("recorded polls must not be poisoned")
            .len()
    }
}

#[async_trait]
impl EventStore for SourcePollRecorder {
    async fn append(
        &self,
        key: &str,
        events: Vec<AppendingEvent>,
    ) -> Result<AppendOutcome, AppendError> {
        self.inner.append(key, events).await
    }

    async fn load(&self, query: LoadQuery) -> Result<EventStream<'_>, LoadError> {
        if query.stream_key.as_deref() == Some(UPSTREAM_KEY) {
            self.polls
                .lock()
                .expect("recorded polls must not be poisoned")
                .push(Instant::now());
        }
        self.inner.load(query).await
    }
}

/// The store the observer's system is built over: the recording journal, plus
/// the saga-offset slot the delivery host every spawn starts records in.
type PollStore = CustomStore<Unprovided, Unprovided, InMemoryOffsetStore>;

/// What one spawned observer leaves for a clause to read.
///
/// The system and the host handle are carried so neither is dropped while the
/// window runs — a dropped system would take the poller down and make an
/// interval look longer than it is.
struct Observer {
    recording: SourcePollRecorder,
    _system: EventSourceSystem<PollStore>,
    _host: SagaHostProxy<DeliverySaga>,
}

/// Spawn a manager observing `Triggered` on a system whose journal records what
/// the source asks for, with that source's interval named by `interval` — or
/// left unnamed by `None`.
async fn observe(interval: Option<Duration>) -> Observer {
    let recording = SourcePollRecorder::new();
    let system = EventSourceSystem::builder(ProcessSystem::new().await)
        .with_persistence(PersistenceProvider::new(
            CustomStore::builder()
                .eventstore(recording.clone())
                .saga_offsets(InMemoryOffsetStore::default())
                .build(),
        ))
        .build();

    let key = AggregateId::new(UPSTREAM_KEY);
    let subscription = Subscription::stream(&key);
    let subscription = match interval {
        Some(interval) => subscription.with_poll_interval(interval),
        None => subscription,
    };

    let host = SagaManagerProps::<DeliverySaga>::new(|_components| DeliverySaga)
        .observing::<Triggered>(subscription)
        .spawn(&system)
        .await;

    Observer {
        recording,
        _system: system,
        _host: host,
    }
}

/// How many polls a source wired at [`SLOW`] manages inside [`WINDOW`].
fn slow_polls_in_window() -> u128 {
    WINDOW.as_millis() / SLOW.as_millis()
}

#[tokio::test]
async fn the_interval_named_on_a_subscription_is_the_interval_between_its_queries() {
    // Given: nothing upstream, so the gaps are the poll interval and not the
    // time taken by work.
    let observer = observe(Some(SLOW)).await;

    // When
    tokio::time::sleep(WINDOW).await;

    // Then
    let gaps = observer.recording.load_gaps();
    assert!(
        gaps.len() >= 2,
        "the source must poll more than once in {WINDOW:?}; it polled {} times",
        observer.recording.load_count()
    );
    for (nth, gap) in gaps.iter().enumerate() {
        assert!(
            *gap >= SLOW - Duration::from_millis(50),
            "gap {nth} was {gap:?}, which is shorter than the {SLOW:?} the \
             subscription named: the interval the binding carried is not \
             reaching this source's poller"
        );
    }
}

#[tokio::test]
async fn a_shorter_interval_polls_more_often_in_the_same_window() {
    // Given
    let observer = observe(Some(TEST_POLL_INTERVAL)).await;

    // When
    tokio::time::sleep(WINDOW).await;

    // Then
    let polls = u128::try_from(observer.recording.load_count()).unwrap_or(u128::MAX);
    assert!(
        polls > slow_polls_in_window() * 3,
        "a source bound with {TEST_POLL_INTERVAL:?} polled {polls} times in \
         {WINDOW:?}, which is not meaningfully more than the {} a source bound \
         with {SLOW:?} manages — the value named is being treated as a hint",
        slow_polls_in_window()
    );
}

#[tokio::test]
async fn naming_no_interval_leaves_the_cadence_the_framework_already_has() {
    // Given: the two bindings differ in nothing but whether an interval is
    // named, so the contrast below is attributable to that alone.
    let unnamed = observe(None).await;
    let named_slow = observe(Some(SLOW)).await;

    // When
    tokio::time::sleep(WINDOW).await;

    // Then: the unnamed source polls, and does so on a cadence of its own
    let unnamed_gaps = unnamed.recording.load_gaps();
    assert!(
        unnamed_gaps.len() >= 2,
        "a source that names no interval must still poll: it polled {} times \
         in {WINDOW:?}, so an unnamed interval is being read as a reason not \
         to ask",
        unnamed.recording.load_count()
    );
    for (nth, gap) in unnamed_gaps.iter().enumerate() {
        assert!(
            *gap < SLOW,
            "gap {nth} of the source that named no interval was {gap:?} — as \
             long as the {SLOW:?} the other binding named.  An interval \
             reaches the source it was named on and no other, and naming none \
             is not a way of naming something"
        );
    }

    // And: the one that named an interval still keeps it in the same window,
    // so the contrast is between the two bindings rather than between two runs
    for (nth, gap) in named_slow.recording.load_gaps().iter().enumerate() {
        assert!(
            *gap >= SLOW - Duration::from_millis(50),
            "gap {nth} of the source naming {SLOW:?} was {gap:?}: the named \
             interval has to hold while an unnamed source runs beside it"
        );
    }
}
