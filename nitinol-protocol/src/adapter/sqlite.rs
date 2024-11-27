use std::collections::BTreeSet;
use async_trait::async_trait;
use sqlx::{Pool, Sqlite, SqliteConnection};
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
    pub async fn setup(pool: Pool<Sqlite>) -> Result<Self, ProtocolError> {
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
            SELECT (id, sequence_id, registry_key, bytes) 
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
            SELECT (id, sequence_id, registry_key, bytes) 
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