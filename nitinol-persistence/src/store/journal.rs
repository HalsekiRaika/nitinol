//! The journal a store answers with, once the backend's type is out of the
//! signature.

use std::fmt;

use crate::error::{AppendError, LoadError};
use crate::event::AppendingEvent;
use crate::provider::Store;
use crate::query::{AppendOutcome, LoadQuery};
use crate::store::event_store::{EventStore, EventStream};

/// The journal registered with the framework, with the backend's type erased.
///
/// What is inside is the registered store's own clone and nothing else: erasing
/// a type does not copy a store's shape or construct a destination, so a fact
/// appended through a journal is a fact in the backend a deployment named. Its
/// own [`Clone`] is that backend's clone one level down — the
/// [`EventStoreClone`](crate::store::EventStoreClone) supertrait is what carries
/// it — so every wiring point the framework hands a journal to reaches the one
/// set of records. That is `PRV-1` as the erasure has to keep it.
///
/// # Why it does not implement `EventStore`
///
/// The delegation below is inherent rather than an [`EventStore`]
/// implementation, which is what makes two things true at once.
///
/// [`From`] has room for the conversion. `impl<E: EventStore> From<E> for
/// Journal` would overlap `core`'s reflexive `impl<T> From<T> for T` at
/// `E = Journal` if a journal were an event store; because it is not, both
/// implementations stand, and `Journal::from` accepts a backend handle *and* a
/// journal.
///
/// And erasing twice is a type error rather than a `Box<Box<_>>`. A journal is
/// not something `From<E: EventStore>` accepts, so the only ways to reach one
/// from another are the reflexive conversion and `From<&Journal>` — both of
/// which are the clone.
///
/// ```compile_fail
/// use nitinol_persistence::store::{EventStore, InMemoryEventStore, Journal};
///
/// fn takes(_: &impl EventStore) {}
///
/// let journal = Journal::from(InMemoryEventStore::default());
/// // a journal is a handle onto an event store, not an event store
/// takes(&journal);
/// ```
///
/// ```compile_fail
/// use nitinol_persistence::store::{InMemoryEventStore, Journal};
///
/// let journal = Journal::from(InMemoryEventStore::default());
/// // erasing an erased handle: there is no second box to put it in
/// let _twice = Journal::from(Box::new(journal));
/// ```
///
/// # Why it implements `Store`
///
/// A store holding the journal alone *is* the journal, so there is no separate
/// erased-store type beside this one. That is what lets wiring whose types are
/// fixed before a deployment's store is known name a single type: a saga's
/// dispatch mapping names its system when the mapping is written, and
/// `nitinol_eventsource`'s `SystemHandle` is the system over a `Journal`.
///
/// The slots beyond the journal are deliberately absent. A journal has no
/// snapshot store and no offset store to answer with, so wiring that needs one
/// is refused while it is compiled rather than at the first save — which is
/// `PRV-3`.
pub struct Journal(Box<dyn EventStore>);

impl Journal {
    /// Appends `events` to the stream identified by `key`.
    ///
    /// [`EventStore::append`]'s clauses, unchanged: this hands the call to the
    /// registered store and its answer back.
    pub async fn append(
        &self,
        key: &str,
        events: Vec<AppendingEvent>,
    ) -> Result<AppendOutcome, AppendError> {
        self.0.append(key, events).await
    }

    /// Serves the stored events a [`LoadQuery`] selects.
    ///
    /// [`EventStore::load`]'s clauses, unchanged.
    pub async fn load(&self, query: LoadQuery) -> Result<EventStream<'_>, LoadError> {
        self.0.load(query).await
    }
}

impl Clone for Journal {
    fn clone(&self) -> Self {
        Self(self.0.clone_box())
    }
}

impl fmt::Debug for Journal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Journal").finish_non_exhaustive()
    }
}

impl<E: EventStore + 'static> From<E> for Journal {
    fn from(store: E) -> Self {
        Self(Box::new(store))
    }
}

/// Taking a journal from a reference to one is cloning it.
///
/// This is the form the framework's entry points are reached in when the caller
/// has to keep its own handle, and it lands where [`Clone`] does.
impl From<&Journal> for Journal {
    fn from(journal: &Journal) -> Self {
        journal.clone()
    }
}

impl Store for Journal {
    fn events(&self) -> Journal {
        self.clone()
    }
}
