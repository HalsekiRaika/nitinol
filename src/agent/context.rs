use std::sync::Arc;
use crate::agent::extension::Extensions;

pub struct Context {
    pub(crate) sequence: i64,
    pub(crate) extension: Arc<Extensions>
}
