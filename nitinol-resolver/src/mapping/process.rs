use crate::mapping::Mapper;
use nitinol_process::{Process, Receptor};

/// Trait for resolving extension handlers for entity in process.
/// 
/// See [`ResolveMapping`](crate::mapping::ResolveMapping)
pub trait WithResolveMapping: 'static + Sync + Send + Sized
where
    Self: Process
{
    fn mapping(mapper: &mut Mapper<Self>, myself: Receptor<Self>);
}
