use nitinol_core::resolver::ResolveMapping;
use crate::channel::Applier;
use crate::Context;
use crate::identifier::ToEntityId;
use crate::refs::Ref;
use crate::registry::{Registry, RegistryError};

pub async fn run<T: ResolveMapping>(
    id: impl ToEntityId,
    entity: T,
    context: Context,
    registry: &Registry
) -> Result<Ref<T>, RegistryError> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Box<dyn Applier<T>>>();

    let refs = Ref { channel: tx };
    registry.register(id.to_entity_id(), refs.clone()).await?;
    
    tokio::spawn(async move {
        let mut entity = entity;
        let mut context = context;
        while let Some(rx) = rx.recv().await {
            if let Err(e) = rx.apply(&mut entity, &mut context).await {
                tracing::error!("{e}");
            }
            
            if !context.is_active() { 
                tracing::warn!("lifecycle ended.");
                break;
            }
        }
    });
    
    Ok(refs)
}