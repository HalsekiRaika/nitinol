use std::collections::BTreeSet;
use std::str::FromStr;
use std::time::Duration;
use async_trait::async_trait;
use sqlx::{Pool, Sqlite, SqliteConnection};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use nitinol_core::identifier::EntityId;
use crate::errors::ProtocolError;
use crate::io::{Reader, Writer};
use crate::Payload;

pub struct SqliteEventStore {
    pool: Pool<Sqlite>
}

impl Clone for SqliteEventStore {
    fn clone(&self) -> Self {
        Self { pool: self.pool.clone() }
    }
}

impl SqliteEventStore {
    /// Nitinol sets up a Journal Database for storing Events.
    ///
    /// Note: Since run our own migration, we must avoid integrating databases.
    pub async fn setup(url: impl AsRef<str>) -> Result<Self, ProtocolError> {
        let opts = SqliteConnectOptions::from_str(url.as_ref())
            .map_err(|e| ProtocolError::Setup(Box::new(e)))?
            .create_if_missing(true);
        
        let pool = SqlitePoolOptions::new()
            .acquire_timeout(
                dotenvy::var("NITINOL_JOURNAL_ACQUIRE_TIMEOUT")
                    .ok()
                    .and_then(|timeout| timeout.parse::<u64>().ok())
                    .map(Duration::from_millis)
                    .unwrap_or(Duration::from_millis(5000))
            )
            .max_connections(
                dotenvy::var("NITINOL_MAX_JOURNAL_CONNECTION")
                    .ok()
                    .and_then(|max| max.parse::<u32>().ok())
                    .unwrap_or(8)
            )
            .connect_with(opts)
            .await
            .map_err(|e| ProtocolError::Setup(Box::new(e)))?;

        sqlx::migrate!("./migrations/sqlite")
            .run(&pool)
            .await
            .map_err(|e| ProtocolError::Write(Box::new(e)))?;

        Ok(Self { pool })
    }
}

#[async_trait]
impl Writer for SqliteEventStore {
    async fn write(&self, aggregate_id: EntityId, payload: Payload) -> Result<(), ProtocolError> {
        let mut con = self.pool.acquire().await
            .map_err(|e| ProtocolError::Write(Box::new(e)))?;
        Internal::write(aggregate_id.as_ref(), payload, &mut con).await
            .map_err(|e| ProtocolError::Write(Box::new(e)))?;
        Ok(())
    }
}

#[async_trait]
impl Reader for SqliteEventStore {
    async fn read(&self, id: EntityId, seq: i64) -> Result<Payload, ProtocolError> {
        let mut con = self.pool.acquire().await
            .map_err(|e| ProtocolError::Read(Box::new(e)))?;
        let payload = Internal::read(id.as_ref(), seq, &mut con).await
            .map_err(|e| ProtocolError::Read(Box::new(e)))?;
        Ok(payload)
    }

    async fn read_to(&self, id: EntityId, from: i64, to: i64) -> Result<BTreeSet<Payload>, ProtocolError> {
        let mut con = self.pool.acquire().await
            .map_err(|e| ProtocolError::Read(Box::new(e)))?;
        let payload = Internal::read_to(id.as_ref(), from, to, &mut con).await
            .map_err(|e| ProtocolError::Read(Box::new(e)))?;
        Ok(payload)
    }
}

struct Internal;

impl Internal {
    pub async fn write(aggregate_id: &str, payload: Payload, con: &mut SqliteConnection) -> Result<(), sqlx::Error> {
        // language=sqlite
        sqlx::query(r#"
            INSERT INTO journal(id, sequence_id, registry_key, bytes) 
            VALUES ($1, $2, $3, $4)
        "#)
            .bind(aggregate_id)
            .bind(payload.sequence_id)
            .bind(&payload.registry_key)
            .bind(&payload.bytes)
            .execute(&mut *con)
            .await?;
        Ok(())
    }
    
    pub async fn read(id: &str, seq: i64, con: &mut SqliteConnection) -> Result<Payload, sqlx::Error> {
        // language=sqlite
        let payload = sqlx::query_as::<_, Payload>(r#"
            SELECT 
                sequence_id, 
                registry_key, 
                bytes
            FROM journal 
            WHERE 
                id LIKE $1 
            AND sequence_id = $2
        "#)
            .bind(id)
            .bind(seq)
            .fetch_one(&mut *con)
            .await?;
        Ok(payload)
    }

    async fn read_to(id: &str, from: i64, to: i64, con: &mut SqliteConnection) -> Result<BTreeSet<Payload>, sqlx::Error> {
        // language=sqlite
        let payload = sqlx::query_as::<_, Payload>(r#"
            SELECT 
                sequence_id, 
                registry_key, 
                bytes
            FROM journal 
            WHERE 
                id LIKE $1 
            AND sequence_id BETWEEN $2 AND $3
        "#)
            .bind(id)
            .bind(from)
            .bind(to)
            .fetch_all(&mut *con)
            .await?;
        
        let payload = payload.into_iter()
            .collect::<BTreeSet<Payload>>();
        
        Ok(payload)
    }
}


#[cfg(test)]
mod tests {
    use serde::{Deserialize, Serialize};
    use nitinol_core::errors::{DeserializeError, SerializeError};
    use nitinol_core::event::Event;
    use crate::adapter::sqlite::{Internal, SqliteEventStore};
    use crate::Payload;

    #[derive(Debug, Clone, Deserialize, Serialize)]
    pub enum TestEvent {
        A,
        B,
        C
    }
    
    impl Event for TestEvent {
        const REGISTRY_KEY: &'static str = "test-event";

        fn as_bytes(&self) -> Result<Vec<u8>, SerializeError> {
            Ok(serde_json::to_vec(self)?)
        }

        fn from_bytes(bytes: &[u8]) -> Result<Self, DeserializeError> {
            Ok(serde_json::from_slice(bytes)?)
        }
    }
    
    impl From<(i64, TestEvent)> for Payload {
        fn from(value: (i64, TestEvent)) -> Self {
            Self {
                sequence_id: value.0,
                registry_key: TestEvent::REGISTRY_KEY.to_string(),
                bytes: value.1.as_bytes().unwrap(),
            }
        }
    }
    
    #[tokio::test]
    async fn all_integration() {
        let ev_store = SqliteEventStore::setup("sqlite::memory:").await.unwrap();
        
        let id = "TestEntity";
        
        let mut xact = ev_store.pool.begin().await.unwrap();
        Internal::write(id, Payload::from((0, TestEvent::A)), &mut xact).await.unwrap();
        Internal::write(id, Payload::from((1, TestEvent::A)), &mut xact).await.unwrap();
        Internal::write(id, Payload::from((2, TestEvent::B)), &mut xact).await.unwrap();
        Internal::write(id, Payload::from((3, TestEvent::C)), &mut xact).await.unwrap();
        xact.commit().await.unwrap();
        
        let mut acquire = ev_store.pool.acquire().await.unwrap();
        let events = Internal::read_to(id, 0, i64::MAX, &mut acquire).await.unwrap();
        
        println!("{:?}", events);
    }
}