//! How an append through a system's journal reaches its projection activator.

use std::sync::{Arc, OnceLock};

use nitinol_persistence::store::AppendObserver;
use nitinol_persistence::{AppendOutcome, AppendingEvent, EventType};
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;

/// What one accepted append tells the activator: where, how far, and of which
/// types. No payload — a projector reads the events from the journal.
pub(crate) struct Appended {
    pub(crate) key: Arc<str>,
    /// The global position of the append's last event.
    pub(crate) head: u64,
    pub(crate) types: Vec<EventType>,
}

/// The observer a system's journal carries: it hands each accepted append to
/// the system's activator, once one is running.
///
/// Before the first projector is spawned there is no activator, and an append
/// is reported to nobody — nothing could be woken by it.
#[derive(Default)]
pub(crate) struct AppendNotifier {
    activator: OnceLock<mpsc::Sender<Appended>>,
}

impl AppendNotifier {
    /// Report every later append to the activator `notices` reaches.
    pub(crate) fn attach(&self, notices: mpsc::Sender<Appended>) {
        self.activator
            .set(notices)
            .expect("a node starts its activator once, so its notifier is attached once");
    }
}

impl AppendObserver for AppendNotifier {
    fn appended(&self, key: &str, events: &[AppendingEvent], outcome: &AppendOutcome) {
        let Some(activator) = self.activator.get() else {
            return;
        };
        let Some(&head) = outcome.assigned_sequences.last() else {
            return;
        };
        let appended = Appended {
            key: Arc::from(key),
            head,
            types: events.iter().map(|event| event.event_type).collect(),
        };
        match activator.try_send(appended) {
            Ok(()) => {}
            // The appender must not wait on the activator. A notice dropped
            // here only delays a wake-up: every projector reads from its own
            // recorded position, and the watcher reads the journal too.
            Err(TrySendError::Full(dropped)) => tracing::debug!(
                stream = %dropped.key,
                head = dropped.head,
                "projection activator: append notice dropped, its queue is full"
            ),
            Err(TrySendError::Closed(_)) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use bytes::Bytes;
    use nitinol_persistence::{Family, TypeName};

    const FIRST: EventType = EventType::new(Family::new("observer.test"), TypeName::new("First"));
    const SECOND: EventType = EventType::new(Family::new("observer.test"), TypeName::new("Second"));

    fn event(sequence: u64, event_type: EventType) -> AppendingEvent {
        AppendingEvent {
            sequence,
            event_type,
            payload: Bytes::from_static(b"payload"),
            occurred_at: jiff::Timestamp::now(),
        }
    }

    fn outcome(assigned_sequences: Vec<u64>) -> AppendOutcome {
        AppendOutcome {
            stream_version: 2,
            assigned_sequences,
        }
    }

    #[test]
    fn an_append_before_any_activator_is_reported_to_nobody() {
        let notifier = AppendNotifier::default();
        notifier.appended("stream", &[event(1, FIRST)], &outcome(vec![7]));
        let (notices, mut received) = mpsc::channel(4);
        notifier.attach(notices);

        assert!(
            received.try_recv().is_err(),
            "an append made before the activator was attached must not reach it"
        );
    }

    #[test]
    fn one_append_is_one_notice_naming_its_last_position_and_every_type() {
        let notifier = AppendNotifier::default();
        let (notices, mut received) = mpsc::channel(4);
        notifier.attach(notices);

        notifier.appended(
            "stream",
            &[event(1, FIRST), event(2, SECOND)],
            &outcome(vec![11, 12]),
        );

        let appended = received
            .try_recv()
            .expect("an accepted append must reach the attached activator");
        assert_eq!(
            (&*appended.key, appended.head, appended.types),
            ("stream", 12, vec![FIRST, SECOND]),
            "the notice must name the stream, the append's last global position and the type \
             of every event in it"
        );
        assert!(
            received.try_recv().is_err(),
            "an append of two events must be one notice, not one per event"
        );
    }
}
