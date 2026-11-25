//! How an append through a system's journal reaches its projection activator.

use std::sync::Arc;

use async_trait::async_trait;
use nitinol_persistence::store::AppendObserver;
use nitinol_persistence::{AppendOutcome, EventType};
use nitinol_runtime::process::ProcessProxy;

use super::ProjectionActivator;

/// What one accepted append tells the activator: where, how far, and of which
/// types. No payload — a projector reads the events from the journal.
pub(crate) struct Appended {
    pub(crate) key: Arc<str>,
    /// The global position of the append's last event.
    pub(crate) head: u64,
    /// The stream's sequence after the append: that of its last event.
    pub(crate) stream_version: u64,
    pub(crate) types: Vec<EventType>,
}

impl Appended {
    /// The notice an append to `key`, accepted as `outcome`, of events of
    /// `types`, is told as; `None` for an append of no events, which moved no
    /// position and so has nothing to wake.
    fn of(key: &str, outcome: &AppendOutcome, types: &[EventType]) -> Option<Self> {
        let &head = outcome.assigned_sequences.last()?;
        Some(Self {
            key: Arc::from(key),
            head,
            stream_version: outcome.stream_version,
            types: types.to_vec(),
        })
    }
}

/// What a system's journal reports its appends to: it tells each accepted
/// append to the system's activator, which runs from the moment the system is
/// built.
///
/// Before any projection is registered, a notice names nothing the activator
/// follows, and wakes nothing.
pub(crate) struct ActivatorNotifier {
    activator: ProcessProxy<ProjectionActivator>,
}

impl ActivatorNotifier {
    /// A notifier telling appends to `activator`.
    pub(crate) fn new(activator: ProcessProxy<ProjectionActivator>) -> Self {
        Self { activator }
    }
}

#[async_trait]
impl AppendObserver for ActivatorNotifier {
    async fn appended(&self, key: &str, outcome: &AppendOutcome, types: &[EventType]) {
        let Some(appended) = Appended::of(key, outcome, types) else {
            return;
        };
        let (stream, head) = (Arc::clone(&appended.key), appended.head);
        // The activator runs as long as the process system does, so it refuses
        // a notice only while that shuts down, its projectors with it. The
        // append itself was accepted, so it is not failed for that.
        if let Err(error) = self.activator.tell(appended).await {
            tracing::debug!(
                stream = %stream,
                head,
                error = %error,
                "projection activator: append notice not delivered, the activator has stopped"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use nitinol_persistence::{Family, TypeName};

    const FIRST: EventType = EventType::new(Family::new("notifier.test"), TypeName::new("First"));
    const SECOND: EventType = EventType::new(Family::new("notifier.test"), TypeName::new("Second"));

    fn outcome(assigned_sequences: Vec<u64>) -> AppendOutcome {
        AppendOutcome {
            stream_version: 3,
            assigned_sequences,
        }
    }

    #[test]
    fn an_append_is_one_notice_naming_its_last_positions_and_every_type() {
        let appended = Appended::of(
            "stream",
            &outcome(vec![11, 12, 13]),
            &[FIRST, SECOND, FIRST],
        )
        .expect("an append of events must be told");

        assert_eq!(
            (
                &*appended.key,
                appended.head,
                appended.stream_version,
                appended.types
            ),
            ("stream", 13, 3, vec![FIRST, SECOND, FIRST]),
            "the notice must name the stream, the append's last global position, the stream's \
             version after it and the type of every event in it, in the order appended"
        );
    }

    #[test]
    fn an_append_of_no_events_is_no_notice() {
        assert!(
            Appended::of("stream", &outcome(Vec::new()), &[]).is_none(),
            "an append that was assigned no position has nothing to wake"
        );
    }
}
