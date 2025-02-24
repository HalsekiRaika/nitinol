use std::collections::BTreeSet;

use nitinol_core::identifier::ToEntityId;
use nitinol_protocol::io::{ReadProtocol, Reader};
use nitinol_protocol::Payload;
use nitinol_resolver::mapping::{Mapper, ResolveMapping};

use crate::errors::{NotCompatible, ProjectionError};
use crate::fixtures::{Fixture, FixtureParts};
use crate::resolver::HANDLER_TYPE;

#[derive(Debug, Clone)]
pub struct EventProjector {
    reader: ReadProtocol,
}

impl EventProjector {
    pub fn new(reader: impl Reader) -> Self {
        Self {
            reader: ReadProtocol::new(reader),
        }
    }
}

impl EventProjector {
    /// Project entities to the latest state using events stored on the journal database.
    ///
    /// # Arguments
    /// - `id`:  The entity id to project.
    /// - `entity`: `(T, i64)` tuple where:
    ///     - `T`: Entity to project.
    ///     - `i64`: Sequence start value for entity T.
    ///     - If `None`, the projector projects the entity from the beginning.
    ///       In that case, [`Projection<E>::first`](crate::projection::Projection::first) is used.
    #[tracing::instrument(skip_all, name = "EventProjector")]
    pub async fn projection_to_latest<T: ResolveMapping>(
        &self,
        id: impl ToEntityId,
        entity: impl Into<Option<(T, i64)>>,
    ) -> Result<(T, i64), ProjectionError> {
        let id = id.to_entity_id();
        let mut mapping = Mapper::default();
        T::mapping(&mut mapping);

        let replay = match entity.into() {
            None => {
                let journal = self.reader.read_to_latest(id.clone(), 0).await?;
                let parts = patch_load(&mapping, journal).await?;
                patch(None, 0, parts).await?
            }
            Some((entity, from)) => {
                let journal = self.reader.read_to_latest(id.clone(), from).await?;
                let parts = patch_load(&mapping, journal).await?;
                patch(Some(entity), from, parts).await?
            }
        };

        let Some(replay) = replay else {
            unreachable!("Failed to replay entity: {:?}", id);
        };

        tracing::info!("Replay Successful reading events: {}", replay.1);
        Ok(replay)
    }
}

async fn patch_load<T: ResolveMapping>(
    mapping: &Mapper<T>,
    journal: BTreeSet<Payload>,
) -> Result<BTreeSet<FixtureParts<T>>, NotCompatible> {
    journal
        .into_iter()
        .map(|payload| {
            let patcher = mapping
                .find(|key| key.event().eq(&payload.registry_key) && key.handler().eq(HANDLER_TYPE))
                .ok_or(NotCompatible {
                    key: payload.registry_key,
                })?;
            Ok(FixtureParts {
                seq: payload.sequence_id,
                created_at: payload.created_at,
                bytes: payload.bytes,
                patcher,
            })
        })
        .collect()
}

async fn patch<T: ResolveMapping>(
    mut entity: Option<T>,
    mut sequence: i64,
    parts: BTreeSet<FixtureParts<T>>,
) -> Result<Option<(T, i64)>, ProjectionError> {
    let fixture = Fixture::new(Some(parts));

    fixture.apply(&mut entity, &mut sequence).await?;

    Ok(entity.map(|entity| (entity, sequence)))
}
