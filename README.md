# Nitinol

An Actor-based Event Sourcing toolkit for Rust.

## クレート構成

| クレート | 説明 |
|---------|------|
| `nitinol-runtime` | Actor プロセスモデル（spawn / tell / ask / stream）|
| `nitinol-persistence` | 永続化抽象 trait + InMemory 実装 |
| `nitinol-eventsource` | CQRS + ES 統合層（Aggregate / Decider / Projector）|

## Phase 2: nitinol-eventsource + nitinol-persistence

Phase 2 では、Event Sourcing に必要なすべての抽象を実装しました。

### nitinol-persistence

イベント・スナップショット・チェックポイントの永続化抽象 (`EventStore`, `SnapshotStore`, `CheckpointStore`) と
InMemory 実装を提供します。

詳細: [nitinol-persistence/README.md](nitinol-persistence/README.md)

### nitinol-eventsource

`nitinol-runtime` の上に CQRS + ES レイヤーを追加します。

- **Aggregate** / **Decider** / **Receive** — 集約の状態遷移とコマンドハンドリング
- **Effect ADT** — `Persist` / `Apply` / `Side` / `Sequence` の宣言的副作用
- **AggregateProxy** — `ask` / `tell` / `exec` の高レベル API
- **EventPersistor** / **SnapshotPersistor** — 永続化プロセス
- **Projector** / **ProjectorProps** — Catch-up + Live 投影
- **Snapshotable** — スナップショットによるリプレイ短縮
- **DeliveryMode** — AtMostOnce / AtLeastOnce / ExactlyOnce

詳細: [nitinol-eventsource/README.md](nitinol-eventsource/README.md)  
設計ガイド: [nitinol-eventsource/docs/DESIGN.md](nitinol-eventsource/docs/DESIGN.md)

### Examples (eventsource)

| # | 名前 | 内容 |
|---|------|------|
| 1 | [basic-aggregate](examples/eventsource/1.basic-aggregate) | 最小構成のカウンタ集約 |
| 2 | [multiple-deciders](examples/eventsource/2.multiple-deciders) | 1集約に複数の Decider 実装 |
| 3 | [projection](examples/eventsource/3.projection) | Projector + Catch-up |
| 4 | [snapshot](examples/eventsource/4.snapshot) | Snapshotable でリプレイ短縮 |
| 5 | [aggregate-communication](examples/eventsource/5.aggregate-communication) | Effect::Side で集約間通信 |
| 6 | [codec-switch](examples/eventsource/6.codec-switch) | カスタムコーデックの差し替え方 |

### Phase 2 スコープ外

以下は明示的に対象外です：

- **Saga / Process Manager** — 補償ロジックが必要な場合は別途実装が必要
- **スキーマ進化** — 将来の TUI ツールで対応予定
- **具体的な永続化バックエンド** — Postgres / SQLite 等はサードパーティ crate として提供
- **ベンチマーク** — 性能評価は別フェーズ

## ライセンス

MIT OR Apache-2.0
