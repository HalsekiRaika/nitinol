use crate::errors::ProjectionError;
use crate::fixture::{Fixture, FixtureParts};
use crate::identifier::{EntityId, ToEntityId};
use crate::mapping::{Mapper, ResolveMapping};
use crate::protocol::errors::ProtocolError;
use crate::protocol::Payload;
use std::collections::BTreeSet;
use std::sync::Arc;

#[async_trait::async_trait]
pub trait ReadProtocol: 'static + Sync + Send {
    async fn read(
        &self,
        id: &EntityId,
        start: i64,
        to: i64,
    ) -> Result<BTreeSet<Payload>, ProtocolError>;
    async fn read_to(&self, id: &EntityId, to: i64) -> Result<BTreeSet<Payload>, ProtocolError> {
        self.read(id, 0, to).await
    }
    async fn read_to_latest(&self, id: &EntityId) -> Result<BTreeSet<Payload>, ProtocolError> {
        self.read_to(id, i64::MAX).await
    }
}

pub struct Projector {
    reader: Arc<dyn ReadProtocol>,
}

pub struct Material<T> {
    entity: Option<T>,
    protocol: Projector,
}

impl Clone for Projector {
    fn clone(&self) -> Self {
        Self {
            reader: Arc::clone(&self.reader),
        }
    }
}

impl Projector {
    pub fn new(provider: impl ReadProtocol) -> Self {
        Self {
            reader: Arc::new(provider),
        }
    }

    pub fn of<T>(&self, entity: Option<T>) -> Material<T> {
        Material {
            entity,
            protocol: self.clone(),
        }
    }
}

impl<T: ResolveMapping> Material<T> {
    pub async fn projection_to_latest(
        mut self,
        id: &impl ToEntityId,
    ) -> Result<Option<(T, i64)>, ProjectionError> {
        let mut mapper = Mapper::default();
        T::mapping(&mut mapper);

        let bin = self
            .protocol
            .reader
            .read_to_latest(&id.to_entity_id())
            .await
            .unwrap();

        let parts = bin
            .into_iter()
            .map(|payload| {
                let refs = mapper
                    .find_by_key(payload.registry_key)
                    .ok_or(ProjectionError::NotCompatible)?;
                Ok(FixtureParts {
                    seq: payload.sequence_id,
                    bytes: payload.bytes,
                    refs,
                })
            })
            .collect::<Result<BTreeSet<FixtureParts<T>>, ProjectionError>>()?;

        let mut sequence = 0;
        let fixture = Fixture::new(Some(parts));

        fixture.apply(&mut self.entity, &mut sequence).await?;

        let projection = self.entity.map(|entity| (entity, sequence));

        Ok(projection)
    }
}
