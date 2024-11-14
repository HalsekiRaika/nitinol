use crate::agent::{Agent, Applier, Context, Registry, RegistryError};
use crate::identifier::EntityId;
use crate::mapping::ResolveMapping;
use tokio::sync::mpsc;
use tracing::Instrument;

pub(crate) async fn spawn<T: ResolveMapping>(
    id: EntityId,
    entity: T,
    ctx: Context,
    registry: &Registry,
) -> Result<Agent<T>, RegistryError> {
    let (tx, mut rx) = mpsc::unbounded_channel::<Box<dyn Applier<T>>>();

    let moved_id = id.clone();
    let agent = Agent::new(tx);

    registry.register(id, agent.clone()).await?;

    tokio::spawn(
        async move {
            let mut entity = entity;
            let mut context = ctx;
            while let Some(rx) = rx.recv().await {
                if let Err(e) = rx.apply(&mut entity, &mut context).await {
                    tracing::error!("{:?}", e);
                }
            }
        }
        .instrument(tracing::info_span!("", id = %moved_id)),
    );

    Ok(agent)
}
