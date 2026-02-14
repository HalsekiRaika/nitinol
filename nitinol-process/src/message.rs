/// A trait that represents low-level messages in a lightweight-process model, 
/// separate from commands and events.
pub trait Message: Sync + Send + 'static {
    
}