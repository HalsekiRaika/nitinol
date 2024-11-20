use std::collections::BTreeSet;
use spectroscopy_core::errors::ProjectionError;
use spectroscopy_core::resolver::{Mapper, ResolveMapping};
use spectroscopy_protocol::io::ReadProtocol;
use spectroscopy_protocol::Payload;
use crate::errors::NotCompatible;
use crate::fixtures::{Fixture, FixtureParts};

pub mod errors;

mod fixtures;

#[derive(Debug, Clone)]
pub struct Projector {
    reader: ReadProtocol
}

impl Projector {
    pub fn new(reader: ReadProtocol) -> Self {
        Self { reader }
    }
}

impl Projector {
    pub async fn projection_to_latest<T: ResolveMapping>(
        &self, 
        id: impl Into<String>, 
        entity: impl Into<Option<(T, i64)>>
    ) -> Result<T, ProjectionError> {
        let id = id.into();
        
        let mut mapping = Mapper::default();
        T::mapping(&mut mapping);
        
        match entity.into() {
            None => {
                let journal = self.reader.read_to_latest(&id, 0).await
                    .map_err(|e| ProjectionError::Protocol(Box::new(e)))?;
                let parts = patch_load(&mapping, journal).await
                    .map_err(|e| ProjectionError::Projection(Box::new(e)))?;
                patch(None, 0, parts).await
            }
            Some((entity, seq)) => {
                let journal = self.reader.read_to_latest(&id, seq).await
                    .map_err(|e| ProjectionError::Protocol(Box::new(e)))?;
                let parts = patch_load(&mapping, journal).await
                    .map_err(|e| ProjectionError::Projection(Box::new(e)))?;
                patch(Some(entity), seq, parts).await
            }
        }
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

    fixture.apply(&mut entity, &mut sequence).await?;

    Ok(entity.map(|entity| (entity, sequence)))
}
