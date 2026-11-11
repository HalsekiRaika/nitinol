//! What a recovery marker asks of the system an aggregate is spawned on.
//!
//! [`Aggregate::Recover`] is declared in `nitinol-contract`, which knows
//! nothing of stores. What each marker *needs* is therefore stated here, where
//! the stores are: [`Never`] needs the journal alone, and [`Latest`] needs the
//! snapshot store too. Both are stated as an implementation of one trait over
//! the store type, so an aggregate spawned onto a store that cannot supply what
//! its marker asks for has no impl to resolve and the wiring does not compile.
//!
//! The one system whose type cannot say is the [`ContingentSystem`] a typed
//! system is viewed as: it carries the snapshot slot as a value, so what each
//! marker needs there is stated by a second trait, [`RecoverOnContingent`].
//! There a [`Latest`] aggregate resolves, and the deployment's answer is read
//! when the reference is built — a deployment without the slot is a
//! [`MissingSnapshotSlot`] every dispatch through that reference answers with.

use std::sync::Arc;

use nitinol_contract::{Aggregate, Latest, Never, Recovery, Snapshotable};
use nitinol_persistence::provider::{SnapshotSlot, Store};

use crate::codec;
use crate::error::{CodecError, MissingSnapshotSlot};
use crate::system::{ContingentSystem, EventSourceSystem, SnapshotSource};

/// How a snapshot payload becomes the aggregate it was captured from.
///
/// Shared because a reference re-resolves its aggregate long after the entry
/// point that produced it returned, and every activation it starts needs the
/// same one.
type RecoverFn<A> = Arc<dyn Fn(&[u8]) -> Result<A, CodecError> + Send + Sync>;

/// What an activation that recovers needs: where this system's one persistor is
/// resolved, and how to read a payload back as the aggregate.
///
/// Carried by the activation rather than resolved at the wiring, because
/// [`EventSourceSystem::aggregate_proxy`] builds a reference without awaiting
/// anything while starting the persistor is asynchronous.
#[doc(hidden)]
pub struct SnapshotRecovery<A> {
    source: SnapshotSource,
    recover: RecoverFn<A>,
}

impl<A> Clone for SnapshotRecovery<A> {
    fn clone(&self) -> Self {
        Self {
            source: self.source.clone(),
            recover: Arc::clone(&self.recover),
        }
    }
}

impl<A> SnapshotRecovery<A> {
    /// The one snapshot persistor the system distributes.
    pub(crate) async fn persistor(&self) -> crate::process::SnapshotPersistorProxy {
        self.source.resolve().await
    }

    /// Read `payload` back as the aggregate it was captured from.
    pub(crate) fn recover(&self, payload: &[u8]) -> Result<A, CodecError> {
        (self.recover)(payload)
    }
}

impl<A: Snapshotable> SnapshotRecovery<A> {
    /// Recover `A` through the persistor `source` resolves.
    fn from_source(source: SnapshotSource) -> Self {
        Self {
            source,
            // No codec argument anywhere: the aggregate named the format
            // alongside the snapshot value it writes.
            recover: Arc::new(|payload: &[u8]| {
                codec::decode::<<A as Snapshotable>::Codec, A::Snapshot>(payload).map(A::recover)
            }),
        }
    }
}

/// Spawning the aggregate `A` onto a system over the store `St`, as its
/// [`Recover`](Aggregate::Recover) marker allows.
///
/// This is the bound every aggregate spawn carries. It is satisfied for
/// [`Never`] on any [`Store`] and for [`Latest`] only on a [`SnapshotSlot`], so
/// "this aggregate recovers from snapshots and this deployment's store holds
/// none" is a trait-bound error at the wiring rather than an activation that
/// quietly replays from the beginning.
///
/// Sealed by its supertrait: [`Recovery`] has no implementors outside
/// `nitinol-contract`, and this trait is this crate's, so the two markers below
/// are the whole of it.
pub trait RecoverOn<A: Aggregate, St: Store>: Recovery<A> {
    /// What the activation carries to recover with, or nothing when the marker
    /// says to replay the whole stream.
    #[doc(hidden)]
    fn recovery(system: &EventSourceSystem<St>) -> Option<SnapshotRecovery<A>>;
}

impl<A: Aggregate, St: Store> RecoverOn<A, St> for Never {
    fn recovery(_system: &EventSourceSystem<St>) -> Option<SnapshotRecovery<A>> {
        None
    }
}

impl<A, St> RecoverOn<A, St> for Latest
where
    A: Aggregate + Snapshotable,
    St: SnapshotSlot,
{
    fn recovery(system: &EventSourceSystem<St>) -> Option<SnapshotRecovery<A>> {
        Some(SnapshotRecovery::from_source(system.snapshot_source()))
    }
}

/// Resolving the aggregate `A` through a [`ContingentSystem`], as its
/// [`Recover`](Aggregate::Recover) marker allows.
///
/// This is the bound the contingent entry points carry, and it is satisfied
/// for both markers: a contingent system does not say in its type whether the
/// deployment holds a snapshot slot, so there is nothing for the compiler to
/// refuse. What [`RecoverOn`] refuses at the typed wiring is answered here
/// when the reference is built — a [`Latest`] aggregate on a deployment
/// without the slot is a [`MissingSnapshotSlot`].
///
/// Sealed by its supertrait, as [`RecoverOn`] is.
pub trait RecoverOnContingent<A: Aggregate>: Recovery<A> {
    /// What the activation carries to recover with, nothing when the marker
    /// says to replay the whole stream, or why the aggregate cannot be
    /// activated on this deployment at all.
    #[doc(hidden)]
    fn recovery(
        system: &ContingentSystem,
    ) -> Result<Option<SnapshotRecovery<A>>, MissingSnapshotSlot>;
}

impl<A: Aggregate> RecoverOnContingent<A> for Never {
    fn recovery(
        _system: &ContingentSystem,
    ) -> Result<Option<SnapshotRecovery<A>>, MissingSnapshotSlot> {
        Ok(None)
    }
}

impl<A> RecoverOnContingent<A> for Latest
where
    A: Aggregate + Snapshotable,
{
    fn recovery(
        system: &ContingentSystem,
    ) -> Result<Option<SnapshotRecovery<A>>, MissingSnapshotSlot> {
        let snapshots = system.snapshot_store().ok_or(MissingSnapshotSlot)?;
        Ok(Some(SnapshotRecovery::from_source(
            system.snapshot_source(snapshots),
        )))
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use nitinol_contract::{Codec, Event};
    use nitinol_persistence::provider::{CustomStore, PersistenceProvider};
    use nitinol_persistence::store::{InMemoryEventStore, InMemoryStore};
    use nitinol_persistence::{EventType, Family, TypeName};
    use nitinol_runtime::ProcessSystem;

    use super::*;

    #[derive(Default)]
    struct Probe;

    #[derive(Clone)]
    struct ProbeEvent;
    struct ProbeCodec;

    impl Codec<ProbeEvent> for ProbeCodec {
        type Error = std::convert::Infallible;

        fn encode(_value: &ProbeEvent) -> Result<Bytes, Self::Error> {
            Ok(Bytes::new())
        }

        fn decode(_payload: &[u8]) -> Result<ProbeEvent, Self::Error> {
            Ok(ProbeEvent)
        }
    }

    impl Event for ProbeEvent {
        const EVENT_TYPE: EventType =
            EventType::new(Family::new("recover.pid.probe"), TypeName::new("probed"));
        type Codec = ProbeCodec;
    }

    impl Aggregate for Probe {
        type Event = ProbeEvent;
        type Recover = Latest;

        fn apply(&mut self, _event: ProbeEvent) {}
    }

    #[derive(Debug, thiserror::Error)]
    #[error("a probe snapshot payload is 8 bytes, got {0}")]
    struct ProbeSnapshotDecodeError(usize);

    impl Codec<u64> for ProbeCodec {
        type Error = ProbeSnapshotDecodeError;

        fn encode(value: &u64) -> Result<Bytes, Self::Error> {
            Ok(Bytes::from(value.to_be_bytes().to_vec()))
        }

        fn decode(payload: &[u8]) -> Result<u64, Self::Error> {
            payload
                .try_into()
                .map(u64::from_be_bytes)
                .map_err(|_| ProbeSnapshotDecodeError(payload.len()))
        }
    }

    impl Snapshotable for Probe {
        type Snapshot = u64;
        type Codec = ProbeCodec;

        fn capture(&self) -> u64 {
            0
        }

        fn recover(_snapshot: u64) -> Self {
            Self
        }
    }

    async fn in_memory_system() -> EventSourceSystem<InMemoryStore> {
        EventSourceSystem::builder(ProcessSystem::new().await)
            .with_persistence(PersistenceProvider::new(InMemoryStore::default()))
            .build()
    }

    /// What an activation actually resolves through [`SnapshotRecovery::persistor`]
    /// — not a second call to the public accessor — is the one persistor process
    /// [`EventSourceSystem::snapshot_persistor`] hands any other caller.
    ///
    /// A regression that resolved a separate process for the recovery path would
    /// still let an aggregate recover correctly from its own snapshot store, so
    /// nothing observable through `GetCount`-style querying would catch it; this
    /// compares the two resolutions directly instead.
    #[tokio::test]
    async fn activation_recovery_resolves_the_same_persistor_the_system_distributes() {
        // Given
        let system = in_memory_system().await;

        // When: the exact recovery a `Latest` aggregate's activation carries
        // resolves its persistor
        let recovery = <Latest as RecoverOn<Probe, InMemoryStore>>::recovery(&system)
            .expect("Latest on a SnapshotSlot store must carry a recovery");
        let activation_pid = recovery.persistor().await.pid();

        // Then: it is the same process the system's public accessor resolves
        assert_eq!(
            activation_pid,
            system.snapshot_persistor().await.pid(),
            "a system distributes one snapshot persistor, so the recovery an \
             activation carries must resolve the same process as \
             EventSourceSystem::snapshot_persistor"
        );
    }

    /// The same clause through the contingent system: the recovery a `Latest`
    /// aggregate resolved there carries resolves the typed system's one
    /// persistor, rather than a second process over the slot the contingent
    /// system carries.
    #[tokio::test]
    async fn contingent_recovery_resolves_the_same_persistor_the_system_distributes() {
        // Given
        let system = in_memory_system().await;
        let contingent = system.to_contingent();

        // When
        let recovery = <Latest as RecoverOnContingent<Probe>>::recovery(&contingent)
            .expect("the contingent system of a deployment holding a snapshot slot is not refused")
            .expect("Latest through the contingent system must carry a recovery");
        let activation_pid = recovery.persistor().await.pid();

        // Then
        assert_eq!(
            activation_pid,
            system.snapshot_persistor().await.pid(),
            "the contingent system shares the typed system's node, so the \
             recovery it builds must resolve the one persistor that system \
             distributes"
        );
    }

    /// Through the contingent system of a deployment without a snapshot slot,
    /// `Latest` answers the refusal rather than a recovery or "replay the whole
    /// stream".
    #[tokio::test]
    async fn latest_through_a_contingent_system_without_a_snapshot_slot_is_refused() {
        // Given
        let store = CustomStore::builder()
            .eventstore(InMemoryEventStore::default())
            .build();
        let system = EventSourceSystem::builder(ProcessSystem::new().await)
            .with_persistence(PersistenceProvider::new(store))
            .build();

        // When
        let recovery = <Latest as RecoverOnContingent<Probe>>::recovery(&system.to_contingent());

        // Then
        assert!(
            matches!(recovery, Err(MissingSnapshotSlot)),
            "a deployment without a snapshot slot must refuse a `Latest` \
             aggregate, not answer `Ok(None)` and have it replay in place of a \
             snapshot"
        );
    }
}
