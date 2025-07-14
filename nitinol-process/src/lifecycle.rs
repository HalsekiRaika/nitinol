use nitinol_core::identifier::ToEntityId;
use crate::channel::ProcessApplier;
use crate::{Process, Context};
use crate::errors::AlreadyExist;
use crate::receptor::Receptor;
use crate::registry::ProcessRegistry;

pub async fn run<T: Process>(
    id: impl ToEntityId,
    entity: T,
    start_seq: i64,
    registry: ProcessRegistry
) -> Result<Receptor<T>, AlreadyExist> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Box<dyn ProcessApplier<T>>>();

    let entity_id = id.to_entity_id();
    let refs = Receptor { channel: tx };
    
    let context = Context::new(start_seq, registry.clone());
    
    #[cfg(tokio_unstable)]
    let named = entity_id.clone();
    
    registry.register(entity_id.clone(), refs.clone()).await?;
    
    let process = async move {
        let id = entity_id;
        let mut entity = entity;
        let mut context = context;
        let registry = registry;
        
        entity.start(&mut context).await;
        
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
        
        entity.stop(&mut context).await;
    };
    
    #[cfg(tokio_unstable)]
    {
        let _ = tokio::task::Builder::new()
            .name(named.as_ref())
            .spawn(process)
            .expect("unexpected error occurred from tokio-runtime.");
    }
    
    #[cfg(not(tokio_unstable))]
    tokio::spawn(process);
    
    Ok(refs)
}
