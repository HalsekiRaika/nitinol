use std::collections::BTreeSet;
use nitinol_core::errors::ProjectionError;
use nitinol_core::identifier::ToEntityId;
use nitinol_core::resolver::{Mapper, ResolveMapping};
use nitinol_protocol::io::{ReadProtocol, Reader};
use nitinol_protocol::Payload;
use crate::errors::{FailedProjection, FailedProjectionWithKey, NotCompatible};
use crate::fixtures::{Fixture, FixtureParts};

pub mod errors;

mod fixtures;

#[derive(Debug, Clone)]
pub struct EventProjector {
    reader: ReadProtocol
}

impl EventProjector {
    pub fn new(reader: impl Reader) -> Self {
        Self { reader: ReadProtocol::new(reader) }
    }
}

impl EventProjector {
    pub async fn projection_to_latest<T: ResolveMapping>(
        &self, 
        id: impl ToEntityId,
        entity: impl Into<Option<(T, i64)>>
    ) -> Result<(T, i64), ProjectionError> {
        let id = id.to_entity_id();
        let mut mapping = Mapper::default();
        T::mapping(&mut mapping);
        
        let replay = match entity.into() {
            None => {
                let journal = self.reader.read_to_latest(id.clone(), 0).await
                    .map_err(|e| ProjectionError::Protocol(Box::new(e)))?;
                let parts = patch_load(&mapping, journal).await
                    .map_err(|e| ProjectionError::Projection(Box::new(e)))?;
                patch(None, 0, parts).await?
                    .ok_or(ProjectionError::Projection(Box::new(FailedProjection { id: id.to_entity_id() })))
            }
            Some((entity, seq)) => {
                let journal = self.reader.read_to_latest(id.clone(), seq).await
                    .map_err(|e| ProjectionError::Protocol(Box::new(e)))?;
                let parts = patch_load(&mapping, journal).await
                    .map_err(|e| ProjectionError::Projection(Box::new(e)))?;
                patch(Some(entity), seq, parts).await?
                    .ok_or(ProjectionError::Projection(Box::new(FailedProjection { id: id.to_entity_id() })))
            }
        }?;
        
        tracing::info!("Replay Successful reading events: {}", replay.1);
        Ok(replay)
    }
    
    #[rustfmt::skip]
    pub async fn projection_with_resolved_events<T: ResolveMapping>(&self, base: T) -> Result<(T, i64), ProjectionError> {
        let mut mapping = Mapper::default();
        T::mapping(&mut mapping);
        
        let mut journal = Vec::new();
        for key in mapping.registry_keys() {
            let chunked = self.reader.read_all_by_key(&key).await
                .map_err(|e| ProjectionError::Protocol(Box::new(e)))?;
            tracing::info!("Read {} payloads for key: {}", chunked.len(), key);
            journal.push(chunked);
        }

        let journal = journal.into_iter()
            .flatten()
            .collect::<BTreeSet<Payload>>();
    
        tracing::debug!("Expand {} payloads", journal.len());
        tracing::trace!("{journal:?}");
    
        let parts = patch_load(&mapping, journal).await
            .map_err(|e| ProjectionError::Projection(Box::new(e)))?;
    
        patch(Some(base), 0, parts).await?
            .ok_or(ProjectionError::Projection(Box::new(FailedProjectionWithKey { keys: mapping.registry_keys().join(", ") })))
    }
}


async fn patch_load<T: ResolveMapping>(
    mapping: &Mapper<T>,
    journal: BTreeSet<Payload>
) -> Result<BTreeSet<FixtureParts<T>>, NotCompatible> {
    journal
        .into_iter()
        .map(|payload| {
            let refs = mapping
                .find_by_key(&payload.registry_key)
                .ok_or(NotCompatible { key: payload.registry_key })?;
            Ok(FixtureParts {
                seq: payload.sequence_id,
                bytes: payload.bytes,
                refs,
            })
        })
        .collect()
}

async fn patch<T: ResolveMapping>(
    mut entity: Option<T>, 
    mut sequence: i64,
    parts: BTreeSet<FixtureParts<T>>
) -> Result<Option<(T, i64)>, ProjectionError> {
    let fixture = Fixture::new(Some(parts));

    tracing::trace!(seq = sequence, "Patching entity with fixture");
    fixture.apply(&mut entity, &mut sequence).await?;

    Ok(entity.map(|entity| (entity, sequence)))
}
