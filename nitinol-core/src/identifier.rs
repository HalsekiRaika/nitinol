use std::fmt::{Debug, Display, Formatter};
use std::hash::{Hash, Hasher};
use std::sync::Arc;


pub struct EntityId(Arc<str>);

impl EntityId {
    pub fn new(id: String) -> EntityId {
        Self(id.into())
    }
}

impl Clone for EntityId {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl Debug for EntityId {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Display for EntityId {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl AsRef<str> for EntityId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl Eq for EntityId {}

impl PartialEq<Self> for EntityId {
    fn eq(&self, other: &Self) -> bool {
        self.0.eq(&other.0)
    }
}

impl Hash for EntityId {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.hash(state);
    }
}

pub trait IntoEntityId: 'static + Sync + Send {
    fn into_entity_id(self) -> EntityId;
}

pub trait ToEntityId: 'static + Sync + Send {
    fn to_entity_id(&self) -> EntityId;
}

impl<T: ToString + Sync + Send + 'static> IntoEntityId for T {
    fn into_entity_id(self) -> EntityId {
        EntityId::new(self.to_string())
    }
}

impl<T: ToString + Sync + Send + 'static + ?Sized> ToEntityId for T {
    fn to_entity_id(&self) -> EntityId {
        EntityId::new(self.to_string())
    }
}
