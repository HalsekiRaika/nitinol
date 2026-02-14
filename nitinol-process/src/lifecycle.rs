use std::time::Duration;
use nitinol_core::identifier::ToEntityId;
use crate::task::TaskApplier;
use crate::{Process, Context};
use crate::errors::AlreadyExist;
use crate::receptor::Receptor;
use crate::registry::ProcessRegistry;

pub async fn run<T: Process>(
    id: impl ToEntityId,
    entity: T,
    start_seq: i64,
    registry: ProcessRegistry,
    timeout: Option<Duration>
) -> Result<Receptor<T>, AlreadyExist> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Box<dyn TaskApplier<T>>>();

    let entity_id = id.to_entity_id();
    let refs = Receptor { channel: tx };
    
    let context = Context::new(start_seq, registry.clone());
    
    #[cfg(tokio_unstable)]
    let named = entity_id.clone();
    
    registry.register(entity_id.clone(), refs.clone()).await?;
    
    let process = async move {
        let id = entity_id;
        let registry = registry;
        let mut state = entity;
        let mut context = context;
        
        state.start(&mut context).await;
        
        state = if let Some(timeout) = timeout {
            loop {
                tokio::pin! {
                    let timeout = tokio::time::sleep(timeout);
                }
                tokio::select! {
                    Some(task) = rx.recv() => {
                        if let Err(e) = task.apply(&mut state, &mut context).await {
                            tracing::error!("{e}");
                            break state;
                        }
                    }
                    _ = &mut timeout => {
                        tracing::info!("Process timeout.");
                        break state;
                    }
                }
            }
        } else {
            loop {
                match rx.recv().await {
                    Some(task) => {
                        if let Err(e) = task.apply(&mut state, &mut context).await {
                            tracing::error!("{e}");
                            break state
                        }
                    }
                    None => break state
                }
            }
        };
        
        
        // while let Some(rx) = rx.recv().await {
        //     entity = match rx.apply(entity, &mut context).await {
        //         Ok(entity) => entity,
        //         Err(e) => {
        //             tracing::error!("{e}");
        //             break;
        //         }
        //     };
        //     
        //     if !context.is_active().await {
        //         tracing::warn!("lifecycle ended.");
        //         break;
        //     }
        // }
        
        state.stop(&mut context).await;
        
        if let Err(e) = registry.deregister(&id).await {
            tracing::error!("{e}");
        }
        
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
