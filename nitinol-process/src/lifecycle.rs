use nitinol_core::identifier::ToEntityId;
use crate::channel::ProcessApplier;
use crate::{Process, Context};
use crate::errors::AlreadyExist;
use crate::refs::Ref;
use crate::registry::ProcessRegistry;

pub async fn run<T: Process>(
    id: impl ToEntityId,
    entity: T,
    context: Context,
    registry: ProcessRegistry
) -> Result<Ref<T>, AlreadyExist> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Box<dyn ProcessApplier<T>>>();

    let entity_id = id.to_entity_id();
    let refs = Ref { channel: tx };
    registry.register(entity_id.clone(), refs.clone()).await?;
    
    tokio::spawn(async move {
        let id = entity_id;
        let mut entity = entity;
        let mut context = context;
        let registry = registry;
        while let Some(rx) = rx.recv().await {
            if let Err(e) = rx.apply(&mut entity, &mut context).await {
                tracing::error!("{e}");
            }
            
            if !context.is_active().await {
                tracing::warn!("lifecycle ended.");
                break;
            }
        }
        
        if let Err(e) = registry.deregister(&id).await {
            tracing::error!("{e}");
        }
    });
    
    Ok(refs)
}
