use std::error::Error;

#[derive(Debug, thiserror::Error)]
#[error("Not found resource aggregate_id:{aggregate_id}")]
pub struct NotFound {
    pub aggregate_id: String
}

impl NotFound {
    pub fn into_boxed(self) -> Box<dyn Error + Sync + Send> {
        Box::new(self)
    }
}