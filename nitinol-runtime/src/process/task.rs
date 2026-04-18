use async_trait::async_trait;
use crate::process::Process;

pub type UserTask<P> = Box<dyn Task<P>>;

#[async_trait]
pub trait Task<P: Process>: 'static + Sync + Send {
    async fn run(self: Box<Self>, state: &mut P) -> Result<(), ()>;
}
