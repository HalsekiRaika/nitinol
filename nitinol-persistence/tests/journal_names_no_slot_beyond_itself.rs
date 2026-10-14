//! A journal answers for the journal and for nothing beside it.
//!
//! ```gherkin
//!   Scenario: the erased handle names no offset slot
//!     Given a `Journal`
//!     When an offset slot's verb is called on it
//!     Then the crate claims neither name for it, so a projector or a saga
//!          attachment wired over the erased handle has no slot to resolve
//! ```
//!
//! `Journal` implements [`Store`](nitinol_persistence::provider::Store) because
//! a store holding the journal alone *is* the journal, and that is what fixes
//! the type-erased system handle on a single type. The slots beyond the journal
//! are the half that has to stay absent: wiring that needs an offset store takes
//! the slot's bound, so an erased handle reaching for one is refused while the
//! wiring is compiled rather than at the first save. That is `PRV-3`, and the
//! erased handle is where it is easiest to give up — implementing a slot for
//! `Journal` to make some wiring compile would answer with a destination the
//! journal was never assembled over.
//!
//! The refusal is the compiler's, so what is settled here is the fact it rests
//! on: that neither offset slot is `Journal`'s. It is read by the second of the
//! two mechanisms the `*_api_removed.rs` tests in this workspace use — a blanket
//! extension trait claims the two slot verbs with a signature a slot could never
//! accept, and the slot traits are in scope. While the crate claims either name
//! for a journal, a call below has two applicable items and this binary fails to
//! build; while it claims neither, the extension is the only candidate and
//! resolves. The `Declined` label is what makes a resolution in the crate's
//! favour fail to compile rather than pass quietly.
//!
//! The snapshot half of the same absence, and the refusal at the wiring points
//! themselves, are fixed where the wiring is — on
//! `EventSourceSystem::erase_store` in `nitinol-eventsource`.
//!
//! Run with:
//! `cargo test -p nitinol-persistence --test journal_names_no_slot_beyond_itself`.

use nitinol_persistence::store::{InMemoryEventStore, Journal};

// In scope so that a slot implemented for a journal is an ambiguity at the call
// sites below rather than a silent pass. Nothing here names their verbs through
// them.
#[allow(unused_imports)]
use nitinol_persistence::provider::{ProjectionOffsetSlot, SagaOffsetSlot};

/// What a downstream crate is free to define for a journal, because the journal
/// holds no slot the framework could claim the name for.
mod probe {
    /// What a probed call answers with once it is the only candidate.
    pub struct Declined {
        pub label: &'static str,
    }

    /// Claims both offset slot verbs.
    ///
    /// One trait for both slots: they are absent for one reason — a journal has
    /// no offset store to answer with — and a journal claiming either would be
    /// the same mistake.
    pub trait NoOffsetSlotsForAJournal {
        fn projection_offsets(&self, label: &'static str) -> Declined {
            Declined { label }
        }

        fn saga_offsets(&self, label: &'static str) -> Declined {
            Declined { label }
        }
    }

    impl<T> NoOffsetSlotsForAJournal for T {}
}

use probe::NoOffsetSlotsForAJournal;

const PROBE_LABEL: &str = "downstream-label";

/// Given a journal, When each offset slot's verb is called on it, Then both
/// calls resolve to the downstream extension.
#[test]
fn the_offset_slot_verbs_of_a_journal_belong_to_a_downstream_definition() {
    let journal = Journal::from(InMemoryEventStore::default());

    let answered = [
        journal.projection_offsets(PROBE_LABEL).label,
        journal.saga_offsets(PROBE_LABEL).label,
    ];

    assert!(
        answered.iter().all(|label| *label == PROBE_LABEL),
        "a journal must name neither offset slot: a projection's applied \
         position and the framework's saga attachments are kept wherever an \
         operator put them, which an erased journal cannot answer for, so \
         wiring that needs one is refused while it is compiled (`PRV-3`) \
         instead of resolving to a destination of the journal's own; got \
         {answered:?}"
    );
}
