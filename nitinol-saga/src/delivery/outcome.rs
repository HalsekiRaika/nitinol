//! What one record, and one turn of a partition's loop, came to.

use nitinol_eventsource::error::Retryability;
use nitinol_persistence::AggregateId;

use crate::delivery::dispatch::DeliveryFailure;
use crate::delivery::id::SagaDeliveryId;

/// What a partition judged about one record of its subscription.
///
/// The cursor and the hold are read off this one value. Deciding them apart —
/// taking "a delivery succeeded" for progress while separately noting that
/// something failed — is how a cursor comes to stand past a record whose work
/// is still owed, leaving that work with nothing pointing at it.
pub(crate) enum RecordOutcome {
    /// Nothing is owed on this record, so the cursor may stand at it.
    ///
    /// Either every delivery it implied landed, it implied none, or it belongs
    /// to another partition.
    Finished { position: u64 },
    /// Something this record implied is still owed, so the cursor stays below
    /// it and the partition will ask for it again.
    Held(Hold),
}

/// What one turn of a partition's loop came to.
pub(crate) enum TickOutcome {
    /// The partition did everything the turn asked of it.
    Clear,
    /// The partition is waiting on something.
    Held(Hold),
}

/// What a partition is waiting on.
///
/// How long it waits before trying again, what it says about itself, and the
/// level it says that at are all read off this one value.
pub(crate) struct Hold {
    /// Where the partition stopped, and what it was doing.
    pub(crate) cause: HoldCause,
    /// Whether doing the same thing again can plausibly come out differently.
    pub(crate) retryability: Retryability,
    /// What the step that did not finish said.
    pub(crate) detail: String,
}

/// Where a partition stopped.
///
/// A partition reads its recorded progress, reads the records above it, reads
/// each one back as the saga's own event, delivers what each implies, and
/// records how far it got. Each variant is one of those steps not finishing.
pub(crate) enum HoldCause {
    /// A delivery the record at `position` implied did not land.
    Delivery { position: u64, target: AggregateId },
    /// The store would not say where this partition had got to, so it does not
    /// know where to resume.
    ProgressUnread,
    /// The store would not serve the records above `position`.
    RecordsUnread { position: u64 },
    /// The record at `position` could not be read back as the saga's own
    /// event, so what it implied is unknown.
    RecordUnreadable { position: u64 },
    /// The store would not take `position` as this partition's progress.
    ///
    /// The deliveries up to it were carried out all the same: what failed is
    /// the cache of where they got to, not the work. Nothing is skipped on
    /// account of it, and the position is offered again.
    ProgressUnrecorded { position: u64 },
}

impl HoldCause {
    /// The position the partition is held at, where it has one.
    fn position(&self) -> Option<u64> {
        match self {
            Self::Delivery { position, .. }
            | Self::RecordsUnread { position }
            | Self::RecordUnreadable { position }
            | Self::ProgressUnrecorded { position } => Some(*position),
            Self::ProgressUnread => None,
        }
    }

    /// What the partition was doing when it stopped.
    fn step(&self) -> &'static str {
        match self {
            Self::Delivery { .. } => "delivering a command the decision implied",
            Self::ProgressUnread => "reading the progress recorded for this partition",
            Self::RecordsUnread { .. } => "reading the records above the position reached",
            Self::RecordUnreadable { .. } => "reading a record back as the saga's own event",
            Self::ProgressUnrecorded { .. } => "recording the progress reached",
        }
    }
}

/// Emit `fields` at the level `retryability` calls for.
///
/// A verdict on a command is something an operator has to act on, so it is an
/// error; a failure of the machinery clears on its own if what broke recovers,
/// so it is a warning. Both are levels an operator watches without having to
/// turn anything on.
macro_rules! at_level_for {
    ($retryability:expr, $($fields:tt)*) => {
        match $retryability {
            Retryability::Permanent => tracing::error!($($fields)*),
            Retryability::Transient => tracing::warn!($($fields)*),
        }
    };
}

impl Hold {
    /// A hold on the delivery `failure` describes.
    ///
    /// The classification is the one the failure was given where the answer
    /// came back, carried here rather than reconstructed.
    pub(crate) fn delivery(position: u64, target: AggregateId, failure: DeliveryFailure) -> Self {
        Self {
            cause: HoldCause::Delivery { position, target },
            retryability: failure.retryability,
            detail: failure.detail,
        }
    }

    /// A hold on a step whose failure says nothing about any command.
    ///
    /// Nothing was decided, so doing the same thing again may well come out
    /// differently once whatever broke recovers.
    pub(crate) fn transient(cause: HoldCause, detail: impl std::fmt::Display) -> Self {
        Self {
            cause,
            retryability: Retryability::Transient,
            detail: detail.to_string(),
        }
    }

    /// A hold on a step that will fail the same way every time it is tried.
    ///
    /// It is still a hold rather than a reason to move on: the record's work is
    /// owed whether or not reading it can ever succeed, and passing over it is
    /// a judgement an operator makes, not one a loop makes for them.
    pub(crate) fn permanent(cause: HoldCause, detail: impl std::fmt::Display) -> Self {
        Self {
            cause,
            retryability: Retryability::Permanent,
            detail: detail.to_string(),
        }
    }

    /// Say that this partition is held, and everything needed to attribute it.
    ///
    /// A silent hold is indistinguishable from an idle host, so every hold is
    /// said out loud: which host, which partition, which position, and whether
    /// the operator is waiting for a recovery or looking at a verdict.
    pub(crate) fn report(&self, delivery_id: &SagaDeliveryId, partition: usize) {
        match &self.cause {
            HoldCause::Delivery { position, target } => at_level_for!(
                self.retryability,
                retryability = ?self.retryability,
                target = target.as_str(),
                global_sequence = *position,
                partition,
                delivery_id = delivery_id.as_str(),
                detail = self.detail.as_str(),
                "saga delivery is held: {}",
                self.cause.step(),
            ),
            cause => at_level_for!(
                self.retryability,
                retryability = ?self.retryability,
                global_sequence = ?cause.position(),
                partition,
                delivery_id = delivery_id.as_str(),
                detail = self.detail.as_str(),
                "saga delivery is held: {}",
                cause.step(),
            ),
        }
    }
}
