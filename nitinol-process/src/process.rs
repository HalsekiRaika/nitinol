use nitinol_core::resolver::ResolveMapping;

pub trait Process: 'static + Sync + Send + Sized where Self: ResolveMapping {}

