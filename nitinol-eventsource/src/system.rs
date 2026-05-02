use std::marker::PhantomData;
use std::sync::Arc;

use nitinol_persistence::AggregateId;
use nitinol_runtime::ProcessSystem;

use crate::aggregate::Aggregate;
use crate::codec::{Codec, ErasedCodec};
use crate::{AggregateProps, AggregateProxy, EventPersistorRef};

/// Marker type indicating no codec has been configured yet.
///
/// `EventSourceSystem<Unset>` has no useful methods — use
/// [`EventSourceSystem::new`] followed by [`EventSourceSystemBuilder::with_codec`]
/// to produce a configured system.
pub struct Unset;

/// Builder for [`EventSourceSystem`].
///
/// Obtained from [`EventSourceSystem::new`]. Call [`with_codec`][Self::with_codec]
/// to bind a codec type, then [`build`][EventSourceSystemBuilder::build] to
/// produce an [`EventSourceSystem`].
///
/// # Builder
/// ```ignore
/// let system = EventSourceSystem::new(ps)
///     .with_codec::<JsonCodec>()
///     .build();
/// ```
pub struct EventSourceSystemBuilder<C = Unset> {
    ps: ProcessSystem,
    _codec: PhantomData<C>,
}

impl EventSourceSystemBuilder<Unset> {
    fn new(ps: ProcessSystem) -> Self {
        Self {
            ps,
            _codec: PhantomData,
        }
    }

    /// Bind codec type `C` to this builder.
    pub fn with_codec<C>(self) -> EventSourceSystemBuilder<C> {
        EventSourceSystemBuilder {
            ps: self.ps,
            _codec: PhantomData,
        }
    }
}

impl<C> EventSourceSystemBuilder<C> {
    /// Finalise construction and return the configured [`EventSourceSystem`].
    pub fn build(self) -> EventSourceSystem<C> {
        EventSourceSystem {
            ps: self.ps,
            _codec: PhantomData,
        }
    }
}

/// System-level orchestrator that binds a [`ProcessSystem`] to a codec type `C`.
///
/// The codec type is fixed at build time via [`EventSourceSystemBuilder::with_codec`]
/// and must implement [`Codec<E>`] for every event and snapshot type used through
/// this system.
pub struct EventSourceSystem<C> {
    ps: ProcessSystem,
    _codec: PhantomData<C>,
}

impl EventSourceSystem<Unset> {
    /// Begin building an [`EventSourceSystem`] backed by the given [`ProcessSystem`].
    ///
    /// Returns an [`EventSourceSystemBuilder`] — intentional factory method.
    #[allow(clippy::new_ret_no_self)]
    pub fn new(ps: ProcessSystem) -> EventSourceSystemBuilder<Unset> {
        EventSourceSystemBuilder::new(ps)
    }
}

impl<C> EventSourceSystem<C> {
    /// Returns a reference to the underlying [`ProcessSystem`].
    pub fn process_system(&self) -> &ProcessSystem {
        &self.ps
    }
}

impl<C> EventSourceSystem<C>
where
    C: Default + Send + Sync + 'static,
{
    /// Return a type-erased codec for type `E`.
    ///
    /// Creates a default-constructed `C` and wraps it as `Arc<dyn ErasedCodec<E>>`.
    pub fn codec<E>(&self) -> Arc<dyn ErasedCodec<E>>
    where
        C: Codec<E>,
        E: Send + Sync + 'static,
    {
        Arc::new(C::default())
    }

    /// Spawn an aggregate process pre-wired with the system event codec.
    pub async fn spawn_aggregate<A>(
        &self,
        id: AggregateId,
        event_ref: EventPersistorRef,
    ) -> AggregateProxy<A>
    where
        A: Aggregate,
        C: Codec<A::Event>,
    {
        self.aggregate_props::<A>(id, event_ref)
            .spawn(&self.ps)
            .await
    }

    /// Return an [`AggregateProps`] pre-wired with the system event codec.
    ///
    /// Use this when additional configuration (e.g. snapshot persistor) is needed
    /// before spawning.
    pub fn aggregate_props<A>(
        &self,
        id: AggregateId,
        event_ref: EventPersistorRef,
    ) -> AggregateProps<A>
    where
        A: Aggregate,
        C: Codec<A::Event>,
    {
        AggregateProps::<A>::new(id, event_ref).with_codec(self.codec::<A::Event>())
    }
}
