use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::sync::Arc;

use crate::errors::ProjectionError;
use crate::resolver::{PatchHandler, ResolveMapping};

pub struct FixtureParts<T: ResolveMapping> {
    pub(crate) seq: i64,
    pub(crate) bytes: Vec<u8>,
    pub(crate) patcher: Arc<dyn PatchHandler<T>>,
}

impl<T: ResolveMapping> Eq for FixtureParts<T> {}

impl<T: ResolveMapping> PartialEq<Self> for FixtureParts<T> {
    fn eq(&self, other: &Self) -> bool {
        self.seq.eq(&other.seq) && self.bytes.eq(&other.bytes)
    }
}

impl<T: ResolveMapping> PartialOrd<Self> for FixtureParts<T> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl<T: ResolveMapping> Ord for FixtureParts<T> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.seq
            .cmp(&other.seq)
            .then_with(|| self.bytes.cmp(&other.bytes))
    }
}

// Todo: Add a snapshot handler
pub struct Fixture<T: ResolveMapping> {
    parts: Option<BTreeSet<FixtureParts<T>>>,
}

impl<T: ResolveMapping> Fixture<T> {
    pub fn new(parts: Option<BTreeSet<FixtureParts<T>>>) -> Fixture<T> {
        Self { parts }
    }
}

impl<T: ResolveMapping> Fixture<T> {
    pub async fn apply(self, entity: &mut Option<T>, seq: &mut i64) -> Result<(), ProjectionError> {
        let Some(fixture_parts) = self.parts else {
            return Ok(());
        };

        for fixture in fixture_parts.into_iter() {
            fixture.patcher.apply(entity, fixture.bytes, seq).await?;
        }

        Ok(())
    }
}
