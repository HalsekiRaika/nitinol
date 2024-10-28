use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::sync::Arc;
use crate::errors::ProjectionError;
use crate::fixture::{Fixture, FixtureParts};
use crate::identifier::{EntityId, ToEntityId};
use crate::mapping::{Mapper, ResolveMapping};
use crate::protocol::errors::ProtocolError;

mod errors;

#[async_trait::async_trait]
pub trait ReadProtocol: 'static + Sync + Send {
    async fn read_to(&self, id: &EntityId, to: i64) -> Result<BTreeSet<Payload>, ProtocolError>;
    async fn read_to_latest(&self, id: &EntityId) -> Result<BTreeSet<Payload>, ProtocolError>;
}

pub trait WriteProtocol: 'static + Sync + Send {
    async fn write(&self, id: &EntityId, seq: i64) -> Result<(), ProtocolError>;
}


pub struct Payload {
    sequence_id: i64,
    registry_key: String,
    bytes: Vec<u8>
}

impl Eq for Payload {}

impl PartialEq<Self> for Payload {
    fn eq(&self, other: &Self) -> bool {
        self.sequence_id.eq(&other.sequence_id)
    }
}

impl PartialOrd<Self> for Payload {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Payload {
    fn cmp(&self, other: &Self) -> Ordering {
        self.sequence_id.cmp(&other.sequence_id)
    }
}

pub struct ProjectionProtocol {
    read: Arc<dyn ReadProtocol>
}

pub struct Shadow<T> {
    entity: Option<T>,
    start_with: i64,
    protocol: ProjectionProtocol
}

pub struct ReadyFix<T: ResolveMapping> {
    shadow: Shadow<T>,
    fixture: Fixture<T>
}

impl Clone for ProjectionProtocol {
    fn clone(&self) -> Self {
        Self {
            read: Arc::clone(&self.read)
        }
    }
}

impl ProjectionProtocol {
    pub fn of<T>(&self, entity: Option<T>, start_with: i64) -> Shadow<T> {
        Shadow { entity, start_with, protocol: self.clone() }
    }
}

impl<T: ResolveMapping> Shadow<T> {
    pub async fn read_to_latest(self, id: &impl ToEntityId) -> ReadyFix<T> {
        let mut mapper = Mapper::default();
        T::mapping(&mut mapper);
        
        let bin = self.protocol.read.read_to_latest(&id.to_entity_id()).await.unwrap();
        let parts = bin.into_iter()
            .map(|payload| (payload.sequence_id, payload.bytes, mapper.find_by_key(payload.registry_key)))
            .map(|(seq, bytes, handler)| (seq, bytes, handler.ok_or(ProjectionError::NotCompatible)))
            .map(|(seq, bytes, handler)| handler.map(|refs| FixtureParts { seq, bytes, refs }))
            .collect::<Result<Vec<FixtureParts<T>>, _>>()
            .ok();
        ReadyFix { shadow: self, fixture: Fixture::new(parts) }
    }
}

impl<T: ResolveMapping> ReadyFix<T> {
    pub async fn fix(self) -> T {
        let mut base = self.shadow.entity;
        let mut seq = self.shadow.start_with;
        self.fixture.apply(&mut base, &mut seq).await.unwrap();
        
        base.unwrap()
    }
}
