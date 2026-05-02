# nitinol-persistence

イベントソーシングの永続化抽象層。
具体的なストレージバックエンドに依存しない trait 定義と、テスト用の `InMemory` 実装を提供します。

## 概要

```
┌────────────────────────────────────┐
│         nitinol-persistence        │
│                                    │
│  EventStore     SnapshotStore      │
│  CheckpointStore (+ DeliveryMode)  │
│                                    │
│  InMemoryEventStore                │
│  InMemorySnapshotStore             │
│  InMemoryCheckpointStore           │
└────────────────────────────────────┘
```

## 主要 Trait

### `EventStore`

イベントの追記と読み出しを抽象化します。

```rust
#[async_trait]
pub trait EventStore: Send + Sync {
    async fn append(
        &self,
        aggregate_id: &AggregateId,
        events: Vec<AppendingEvent>,
    ) -> Result<AppendOutcome, AppendError>;

    async fn load(&self, query: LoadQuery) -> Result<EventStream<'_>, LoadError>;
}
```

### `SnapshotStore`

集約のスナップショットを保存・取得します。

```rust
#[async_trait]
pub trait SnapshotStore: Send + Sync {
    async fn save(&self, snapshot: PersistedSnapshot) -> Result<(), SnapshotError>;
    async fn load_latest(&self, aggregate_id: &AggregateId) -> Result<Option<PersistedSnapshot>, SnapshotError>;
}
```

### `CheckpointStore`

Projector の処理位置（チェックポイント）を管理します。

```rust
#[async_trait]
pub trait CheckpointStore: Send + Sync {
    type Tx: Send;
    async fn load(&self, projection_id: &ProjectionId) -> Result<Option<u64>, CheckpointError>;
    async fn save(
        &self,
        projection_id: &ProjectionId,
        sequence: u64,
        tx: Option<&mut Self::Tx>,
    ) -> Result<(), CheckpointError>;
}
```

### `DeliveryMode`

Projector のデリバリーセマンティクスを制御します。

| モード | 意味 | チェックポイント保存タイミング |
|--------|------|-------------------------------|
| `AtMostOnce` | 最大1回配送（失敗しても再試行しない）| project() **前** |
| `AtLeastOnce` | 最低1回配送（失敗時は再処理）| project() **後** |
| `ExactlyOnce` | ユーザー自身が project() 内でチェックポイントを保存 | ユーザー実装 |

## InMemory 実装

テストおよびプロトタイプ用のメモリ内実装が付属しています。

```rust
use nitinol_persistence::store::{
    InMemoryEventStore,
    InMemorySnapshotStore,
    InMemoryCheckpointStore,
};
```

本番環境では、外部のデータベース（Postgres、SQLite 等）を使った実装を
サードパーティ crate として提供することを推奨します。

## サードパーティ実装の作り方

`EventStore` trait を実装するだけで `nitinol-eventsource` と統合できます。

```rust
use async_trait::async_trait;
use nitinol_persistence::{
    AggregateId, AppendingEvent, AppendOutcome, LoadQuery,
    error::{AppendError, LoadError},
    store::{EventStore, EventStream},
};

pub struct PostgresEventStore { /* ... */ }

#[async_trait]
impl EventStore for PostgresEventStore {
    async fn append(
        &self,
        aggregate_id: &AggregateId,
        events: Vec<AppendingEvent>,
    ) -> Result<AppendOutcome, AppendError> {
        // INSERT INTO events ...
        todo!()
    }

    async fn load(&self, query: LoadQuery) -> Result<EventStream<'_>, LoadError> {
        // SELECT * FROM events WHERE ...
        todo!()
    }
}
```

## スコープ外

以下はこのクレートの対象外です：

- **具体的な永続化バックエンド**（Postgres / SQLite / DynamoDB 等）— サードパーティ実装として提供
- **スキーマ進化** — 将来の TUI ツールで対応予定
- **マイグレーション** — ユーザーが独自に実装
