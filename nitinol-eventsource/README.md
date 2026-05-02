# nitinol-eventsource

CQRS + Event Sourcing の統合層。`nitinol-runtime` の Actor プロセスモデルの上に、
Event Sourcing に必要なすべての抽象（Aggregate, Decider, Projector, Snapshot など）を提供します。

## 概要

```
┌─────────────────────────────────────────────────┐
│              nitinol-eventsource                │
│                                                 │
│  EventSourceSystem  ──▶  AggregateProcess       │
│       │                       │                 │
│       └─ ProjectorProcess ◀───┤ EventPersistor  │
│                                   SnapshotPersistor │
└─────────────────────────────────────────────────┘
        │                    │
  nitinol-runtime      nitinol-persistence
```

## 主要 Trait 一覧

### `Aggregate`

集約の状態遷移を定義します。`apply` は純粋関数で、副作用を持ちません。

```rust
pub trait Aggregate: Default + Send + Sync + 'static {
    type Event: Event;
    fn apply(&mut self, event: Self::Event);
}
```

### `Decider<C>`

コマンドから `Effect<E>` を生成します。
決定ロジックはすべてここに集約されます。

```rust
#[async_trait]
pub trait Decider<C>: Aggregate {
    type Rejection: std::error::Error + Send + Sync + 'static;
    async fn decide(&self, cmd: C, ctx: &mut Context) -> Result<Effect<Self::Event>, Self::Rejection>;
}
```

### `Receive<M>`

読み取り専用クエリ（副作用なし）を処理します。

```rust
#[async_trait]
pub trait Receive<M>: Aggregate {
    type Response: Send + Sync + 'static;
    type Error: std::error::Error + Send + Sync + 'static;
    async fn recv(&self, msg: M, ctx: &mut Context) -> Result<Self::Response, Self::Error>;
}
```

### `Projector<E>`

イベントをリードモデルに反映します。
`ProjectorProcess` がデリバリーモード制御・チェックポイント管理を行います。

```rust
#[async_trait]
pub trait Projector<E: Event, Tx = ()>: Send + Sync + 'static {
    type Error: std::error::Error + Send + Sync + 'static;
    async fn project(&mut self, event: E, ctx: &mut ProjectionContext<'_, Tx>) -> Result<(), Self::Error>;
}
```

### `Snapshotable`

集約のスナップショット（チェックポイント）を取得・復元します。
実装するとリプレイ時に全イベントを再生せずに済みます。

```rust
pub trait Snapshotable: Sized {
    type Snapshot;
    fn capture(&self) -> Self::Snapshot;
    fn restore(snapshot: Self::Snapshot) -> Self;
}
```

## Effect ADT

`decide()` の戻り値。集約が「何をするか」を宣言的に記述します。

| バリアント | 意味 |
|-----------|------|
| `Effect::None` | 何もしない（Monoid の単位元）|
| `Effect::Persist(events)` | イベントを永続化してから `apply` |
| `Effect::Apply(events)` | `apply` のみ（永続化しない）|
| `Effect::Side(effect)` | 任意の副作用（fire-and-forget）|
| `Effect::Sequence(vec)` | 複数の Effect を順序付きで実行 |

### Effect の組み合わせ

```rust
// 複数のイベントを一度に永続化
Effect::persist_all(vec![EventA, EventB])

// Persist + Side を組み合わせる
Effect::persist(MyEvent).combine(Effect::tell(proxy, SomeCommand))

// Effect は Monoid: combine は連想律を満たす
```

### SideEffect

`Effect::Side` のペイロードは `SideEffect` トレイトを実装します。
ask() の呼び出し元には Side の成否は伝わりません（fire-and-forget）。

```rust
pub trait SideEffect: Send + Sync + 'static {
    fn execute(self: Box<Self>) -> BoxFuture<'static, Result<(), SideEffectError>>;
}
```

## AggregateProxy の使い方

```rust
// ask: コマンドを送信し、永続化されたイベントを受け取る
let events: Vec<MyEvent> = proxy.ask(MyCommand { ... }).await?;

// tell: 返答を待たずにコマンドを送信（fire-and-forget）
proxy.tell(MyCommand { ... }).await?;

// exec: 読み取り専用クエリ（状態を変更しない）
let value: MyResponse = proxy.exec(MyQuery).await?;
```

## セットアップ例

```rust
use std::sync::Arc;
use nitinol_eventsource::{system::EventSourceSystem, EventPersistor};
use nitinol_persistence::store::InMemoryEventStore;
use nitinol_runtime::ProcessSystem;

let ps = ProcessSystem::new().await;
let system = EventSourceSystem::new(ps)
    .with_codec::<JsonCodec>()
    .build();

let event_ref = EventPersistor::spawn(
    system.process_system(),
    Arc::new(InMemoryEventStore::default()),
).await;

let proxy = system
    .spawn_aggregate::<Counter>(AggregateId::new("counter-1"), event_ref)
    .await;

proxy.ask(Increment).await?;
```

## Examples

| # | 名前 | 内容 |
|---|------|------|
| 1 | [basic-aggregate](../examples/eventsource/1.basic-aggregate) | 最小構成のカウンタ集約 |
| 2 | [multiple-deciders](../examples/eventsource/2.multiple-deciders) | 1集約に複数の `Decider<C>` 実装 |
| 3 | [projection](../examples/eventsource/3.projection) | Projector + Catch-up |
| 4 | [snapshot](../examples/eventsource/4.snapshot) | Snapshotable でリプレイ短縮 |
| 5 | [aggregate-communication](../examples/eventsource/5.aggregate-communication) | `Effect::Side` で集約間通信 |
| 6 | [codec-switch](../examples/eventsource/6.codec-switch) | カスタムコーデックの差し替え方 |

## スコープ外

以下は本クレートの対象外です：

- **Saga / Process Manager** — 補償ロジックが必要な場合は別途実装が必要
- **スキーマ進化** — 将来の TUI ツールで対応予定
- **具体的な永続化バックエンド** — Postgres/SQLite 等はサードパーティ実装を使用

## 設計ガイド

詳細な設計判断については [`docs/DESIGN.md`](docs/DESIGN.md) を参照してください。
