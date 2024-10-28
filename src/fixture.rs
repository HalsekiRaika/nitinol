use std::sync::Arc;
use crate::errors::ProjectionError;
use crate::handler::Handler;
use crate::mapping::ResolveMapping;

pub struct Fixture<T: ResolveMapping> {
    parts: Option<Vec<FixtureParts<T>>>
}

impl<T: ResolveMapping> Fixture<T> {
    pub fn new(parts: Option<Vec<FixtureParts<T>>>) -> Fixture<T> {
        Self { parts }
    }
}

pub struct FixtureParts<T: ResolveMapping> {
    pub(crate) seq: i64,
    pub(crate) bytes: Vec<u8>,
    pub(crate) refs: Arc<dyn Handler<T>>
}

impl<T: ResolveMapping> Fixture<T> {
    pub async fn apply(self, entity: &mut Option<T>, seq: &mut i64) -> Result<(), ProjectionError> {
        let Some(fixture_parts) = self.parts else {
            return Ok(())
        };
        
        for fixture in fixture_parts {
            fixture.refs.apply(entity, fixture.bytes, seq).await?;
            *seq += 1;
        }
        
        Ok(())
    }
}