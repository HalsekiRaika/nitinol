//! The journal a store answers with, once the backend's type is out of the
//! signature.

use std::fmt;
use std::sync::Arc;

use nitinol_contract::Event;

use crate::error::{AppendError, LoadError};
use crate::event::AppendingEvent;
use crate::provider::Store;
use crate::query::{AppendOutcome, LoadQuery, Typed};
use crate::store::event_store::{EventStore, EventStream};
use crate::store::typed_load::{TypedEventStream, TypedLoad};

/// The journal registered with the framework, with the backend's type erased.
///
/// - It holds the registered store's own clone and nothing else, so a fact
///   appended through a journal is a fact in the backend a deployment named.
/// - Its [`Clone`] is that backend's clone, carried by the
///   [`EventStoreClone`](crate::store::EventStoreClone) supertrait, so every
///   wiring point the framework hands a journal to reaches the one set of
///   records. That is `PRV-1` as the erasure has to keep it.
/// - It is not an [`EventStore`]. Erasing a journal twice is a type error; the
///   only ways from one journal to another are the reflexive conversion and
///   `From<&Journal>`, both of which clone.
/// - It is a [`Store`]: a store holding the journal alone is the journal, so a
///   journal handed to a
///   [`PersistenceProvider`](crate::provider::PersistenceProvider) is
///   distributed as itself.
/// - A journal implements none of the slot traits. It has no snapshot store and
///   no offset store to answer with, so wiring that needs one is refused while
///   it is compiled rather than at the first save — which is `PRV-3`.
///
/// A journal is a handle onto an event store, not an event store:
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
pub struct Journal {
    store: Box<dyn EventStore>,
    observer: Option<Arc<dyn AppendObserver>>,
}

/// Told about every append a [`Journal`] observed by it handed back as
/// accepted.
///
/// [`appended`](AppendObserver::appended) is called once per accepted append,
/// after the backend answered `Ok` and before the journal returns that answer,
/// with the key and events the append was made with and the outcome the
/// backend assigned. A refused append is never reported, so whatever an
/// observer is told about is already in the journal for a reader to load. It
/// runs on the appender's task, inside its `append` call, so an observer must
/// not block: hand the fact on and return.
pub trait AppendObserver: Send + Sync {
    /// The append of `events` to the stream `key` was accepted as `outcome`.
    fn appended(&self, key: &str, events: &[AppendingEvent], outcome: &AppendOutcome);
}

impl Journal {
    /// This journal, with every append it accepts reported to `observer`.
    ///
    /// The observer replaces any the journal already had, and travels with
    /// every [`Clone`] of the journal returned, so an append made through a
    /// clone is reported too. A journal taken from a store with
    /// [`From`] has no observer, and its appends are reported to nobody.
    #[must_use]
    pub fn observed_by(self, observer: Arc<dyn AppendObserver>) -> Journal {
        Journal {
            store: self.store,
            observer: Some(observer),
        }
    }

    /// Appends `events` to the stream identified by `key`.
    ///
    /// [`EventStore::append`]'s clauses, unchanged: this hands the call to the
    /// registered store and its answer back. If the journal is
    /// [observed](Journal::observed_by), an accepted append is reported to the
    /// observer before its answer is returned.
    ///
    /// # Errors
    ///
    /// - [`AppendError::SequenceConflict`] if a `sequence` is already taken in
    ///   the stream or repeated within the batch; see [`EventStore::append`]'s
    ///   OCC-1.
    /// - [`AppendError::Backend`] if the backend fails, including once its
    ///   bounded re-runs are exhausted; see [`EventStore::append`]'s OCC-4.
    pub async fn append(
        &self,
        key: &str,
        events: Vec<AppendingEvent>,
    ) -> Result<AppendOutcome, AppendError> {
        let Some(observer) = &self.observer else {
            return self.store.append(key, events).await;
        };
        // The backend takes the batch by value, so what the observer is told
        // about has to be kept back before it is handed over.
        let reported = events.clone();
        let outcome = self.store.append(key, events).await?;
        observer.appended(key, &reported, &outcome);
        Ok(outcome)
    }

    /// Serves the stored events a [`LoadQuery`] selects.
    ///
    /// [`EventStore::load`]'s clauses, unchanged.
    ///
    /// # Errors
    ///
    /// Returns [`LoadError::Backend`] if the registered store fails to serve
    /// the query.
    pub async fn load(&self, query: LoadQuery) -> Result<EventStream<'_>, LoadError> {
        self.store.load(query).await
    }
}

/// [`TypedLoad::read`]'s clauses, unchanged: a journal is not an event store,
/// so the read is handed to the registered store it erases.
impl TypedLoad for Journal {
    async fn read<E: Event>(
        &self,
        query: LoadQuery<Typed<E>>,
    ) -> Result<TypedEventStream<'_, E>, LoadError> {
        self.store.read(query).await
    }
}

impl Clone for Journal {
    fn clone(&self) -> Self {
        Self {
            store: self.store.clone_box(),
            observer: self.observer.clone(),
        }
    }
}

impl fmt::Debug for Journal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Journal").finish_non_exhaustive()
    }
}

impl<E: EventStore + 'static> From<E> for Journal {
    fn from(store: E) -> Self {
        Self {
            store: Box::new(store),
            observer: None,
        }
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

#[cfg(doctest)]
mod compile_fail_fixtures {
    /// ```compile_fail
    /// use nitinol_persistence::store::{InMemoryEventStore, Journal};
    ///
    /// let journal = Journal::from(InMemoryEventStore::default());
    /// // erasing an erased handle: there is no second box to put it in
    /// let _twice = Journal::from(Box::new(journal));
    /// ```
    fn journal_cannot_be_erased_twice() {}
}
