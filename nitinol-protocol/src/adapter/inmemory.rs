mod row;
mod lock;

use self::row::Row;
use self::lock::OptLock;
use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use async_trait::async_trait;
use tokio::sync::RwLock;
use crate::adapter::errors::NotFound;
use crate::errors::ProtocolError;
use crate::io::{Reader, Writer};
use crate::Payload;

pub struct InMemoryEventStore {
    journal: Arc<RwLock<HashMap<String, OptLock<BTreeSet<Row>>>>>
}

impl Clone for InMemoryEventStore {
    fn clone(&self) -> Self {
        Self { journal: Arc::clone(&self.journal) }
    }
}

impl Default for InMemoryEventStore {
    fn default() -> Self {
        Self {
            journal: Arc::new(RwLock::new(HashMap::new()))
        }
    }
}

#[async_trait]
impl Writer for InMemoryEventStore {
    async fn write(&self, aggregate_id: &str, payload: Payload) -> Result<(), ProtocolError> {
        let guard = self.journal.read().await;
        if !guard.contains_key(aggregate_id) { 
            let mut guard = self.journal.write().await;
            let mut init = BTreeSet::new();
            init.insert(Row {
                seq: payload.sequence_id,
                registry_key: payload.registry_key,
                bytes: payload.bytes,
            });
            guard.insert(aggregate_id.to_string(), OptLock::new(init));
            return Ok(())
        }
        
        let lock = guard.get(aggregate_id)
            .ok_or(ProtocolError::Read(NotFound { aggregate_id: aggregate_id.to_string() }.into_boxed()))?;
        let mut lock = lock.write().await
            .map_err(|e| ProtocolError::Write(Box::new(e)))?;
        
        lock.insert(Row {
            seq: payload.sequence_id,
            registry_key: payload.registry_key,
            bytes: payload.bytes,
        });
        
        Ok(())
    }
}

#[async_trait]
impl Reader for InMemoryEventStore {
    async fn read(&self, id: &str, seq: i64) -> Result<Payload, ProtocolError> {
        let guard = self.journal.read().await;
        let lock = guard.get(id)
            .ok_or(ProtocolError::Read(NotFound { aggregate_id: id.to_string() }.into_boxed()))?;
        let found = loop {
            match lock.read().await {
                Ok(guard) => {
                    let found = guard.iter().find(|row| row.seq.eq(&seq)).cloned();
                    match guard.sync().await {
                        Ok(_) => break found,
                        Err(e) => {
                            tracing::error!("{}", e);
                            continue;
                        }
                    }
                }
                Err(e) => { 
                    tracing::error!("{}", e);
                    continue;
                }
            }
        };
        
        found
            .map(|row| Payload {
                sequence_id: row.seq,
                registry_key: row.registry_key,
                bytes: row.bytes,
            })
            .ok_or(ProtocolError::Read(NotFound { aggregate_id: id.to_string() }.into_boxed()))
    }

    async fn read_to(&self, id: &str, from: i64, to: i64) -> Result<BTreeSet<Payload>, ProtocolError> {
        let guard = self.journal.read().await;
        let lock = guard.get(id)
            .ok_or(ProtocolError::Read(NotFound { aggregate_id: id.to_string() }.into_boxed()))?;
        let found = loop {
            match lock.read().await {
                Ok(guard) => {
                    let found = guard.iter()
                        .filter(|row| from >= row.seq && row.seq <= to)
                        .cloned()
                        .collect::<BTreeSet<_>>();
                    match guard.sync().await {
                        Ok(_) => break found,
                        Err(e) => {
                            tracing::error!("{}", e);
                            continue;
                        }
                    }
                }
                Err(e) => {
                    tracing::error!("{}", e);
                    continue;
                }
            }
        };

        Ok(found
            .into_iter()
            .map(|row| Payload {
                sequence_id: row.seq,
                registry_key: row.registry_key,
                bytes: row.bytes,
            }) 
            .collect())
    }
}

