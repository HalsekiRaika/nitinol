use crate::queue::errors::PushFailure;
use async_channel::{self, Receiver, Sender};
use std::sync::Arc;
use tokio::sync::RwLock;

#[derive(Debug, Clone)]
pub struct UnboundedQueue<T> {
    pub(crate) tx: Sender<T>,
    pub(crate) rx: UnboundedQueueReader<T>
}

#[derive(Debug)]
pub struct UnboundedQueueReader<T> {
    rx: Receiver<T>,
    count: Arc<RwLock<usize>>,
    is_closed: Arc<RwLock<bool>>
}

impl<T> Default for UnboundedQueue<T> {
    fn default() -> Self {
        let (tx, rx) = async_channel::unbounded();
        Self {
            tx,
            rx: UnboundedQueueReader {
                rx,
                count: Arc::new(RwLock::new(0)),
                is_closed: Arc::new(RwLock::new(false))
            }
        }
    }
}

impl<T> Clone for UnboundedQueueReader<T> {
    fn clone(&self) -> Self {
        Self {
            rx: self.rx.clone(),
            count: Arc::clone(&self.count),
            is_closed: Arc::clone(&self.is_closed),
        }
    }
}

impl<T> UnboundedQueue<T> {
    pub async fn push(&self, item: T) -> Result<(), PushFailure<T>> {
        let is_closed = self.rx.is_closed.read().await;
        if *is_closed {
            return Err(PushFailure(item));
        }

        self.tx.send(item).await
            .map_err(|e| PushFailure(e.0))?;

        let mut current = self.rx.count.write().await;
        let count = current.saturating_add(1);
        *current = count;
        Ok(())
    }

    pub async fn len(&self) -> usize {
        *self.rx.count.read().await
    }
    
    pub async fn is_empty(&self) -> bool {
        self.len().await == 0
    }


    pub async fn clear(&self) {
        let mut count = self.rx.count.write().await;
        let mut is_closed = self.rx.is_closed.write().await;
        *count = 0;
        *is_closed = true;
        self.rx.rx.close();
        self.tx.close();
    }
    
    pub fn receiver(&self) -> UnboundedQueueReader<T> {
        self.rx.clone()
    }
}

impl<T> UnboundedQueueReader<T> {
    pub async fn poll(&mut self) -> Option<T> {
        let pop = self.rx.recv().await.ok();
        if pop.is_some() {
            let mut current = self.count.write().await;
            let count = current.saturating_sub(1);
            *current = count;
        }
        pop
    }
    
    pub async fn len(&self) -> usize {
        *self.count.read().await
    }

    pub async fn is_empty(&self) -> bool {
        self.len().await == 0
    }
}

pub mod errors {
    #[derive(Debug, thiserror::Error)]
    #[error("sending into a closed channel")]
    pub struct PushFailure<T>(pub T);
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn test_send_recv() {
        let queue: UnboundedQueue<String> = UnboundedQueue::default();
        let mut rx = queue.receiver();
        
        tokio::spawn(async move {
            print!("{} -> ", rx.len().await);
            while let Some(recv) = rx.poll().await {
                println!("{}, {recv}", rx.len().await);
                
                if recv.eq_ignore_ascii_case("break") {
                    break;
                }
                print!("{} -> ", rx.len().await);
            }
            println!("stopped");
        });
        
        queue.push("a".to_string()).await.unwrap();
        queue.push("a".to_string()).await.unwrap();
        queue.push("a".to_string()).await.unwrap();
        queue.push("a".to_string()).await.unwrap();
        queue.push("break".to_string()).await.unwrap();
        
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}