mod projection;

use std::any::TypeId;
use std::collections::HashMap;
use std::sync::Arc;

use crate::event::Event;
use crate::projection::Projection;

pub use self::projection::*;

pub trait ResolveMapping: 'static + Sync + Send + Sized {
    fn mapping(mapper: &mut Mapper<Self>);
}

pub struct Mapper<T: ResolveMapping>(HashMap<RegistryKey, Arc<dyn PatchHandler<T>>>);

#[derive(Debug, Copy, Clone, Hash, Eq, PartialEq)]
pub struct RegistryKey(TypeId, &'static str);

impl RegistryKey {
    pub fn new<T: 'static>(key: &'static str) -> RegistryKey {
        Self(TypeId::of::<T>(), key)
    }
}

impl PartialEq<str> for RegistryKey {
    fn eq(&self, other: &str) -> bool {
        self.1.eq(other)
    }
}

impl<T: ResolveMapping> Mapper<T> {
    pub fn find_by_key(&self, k: impl AsRef<str>) -> Option<Arc<dyn PatchHandler<T>>> {
        self.0
            .iter()
            .find(|(key, _)| key.1.eq(k.as_ref()))
            .map(|(_, handler)| handler)
            .cloned()
    }
    
    pub fn registry_keys(&self) -> Vec<String> {
        self.0
            .keys()
            .map(|composition| composition.1.to_string())
            .collect()
    }
}

impl<T: ResolveMapping> Mapper<T> {
    pub fn register<E: Event>(&mut self) -> &mut Self
    where
        T: Projection<E>,
    {
        self.0.insert(
            RegistryKey::new::<E>(E::REGISTRY_KEY),
            Arc::new(ProjectionResolver::default()),
        );
        self
    }
}

impl<T: ResolveMapping> Default for Mapper<T> {
    fn default() -> Self {
        Self(HashMap::new())
    }
}
