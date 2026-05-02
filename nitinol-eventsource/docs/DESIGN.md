# nitinol-eventsource 設計ガイド

このドキュメントは `nitinol-eventsource` の設計判断とその背景を記述します。
「なぜこうなっているか」を理解することで、ライブラリを正しく使えるようになります。

## Decider Pattern

### なぜ `decide()` は `&self` か

`decide()` は集約の**現在の状態**を読み、**これから何が起きるか**を決定します。
この判断は純粋関数であるべきです。

```rust
async fn decide(&self, cmd: C, ctx: &mut Context) -> Result<Effect<E>, Self::Rejection>
```

`&self`（共有参照）なので、決定中に状態が変化することはありません。
状態変化は `apply()` の責務です。

### なぜ `apply()` は sync か

`apply()` は純粋な状態遷移です。I/O は不要なので async にする必要がありません。
また、async にすると `Aggregate` の派生トレイト実装が煩雑になります。

```rust
fn apply(&mut self, event: Self::Event);
```

### なぜ Reply 概念がないか

`decide()` の戻り値は `Effect<E>` であり、Reply ではありません。
呼び出し元は `ask()` の戻り値（永続化されたイベントのリスト）から必要な情報を得ます。

これにより「Decider は何が起きたかを宣言するだけ」という責務が明確になります。
呼び出し元が「何が起きたか」を知りたければ、返ってきたイベントを見ればよいです。

## Effect ADT の使い方と落とし穴

### Effect は Monoid

`Effect::None` が単位元、`combine` が結合演算です。

```rust
let effect = Effect::persist(EventA)
    .combine(Effect::persist(EventB))
    .combine(Effect::Side(Box::new(my_side_effect)));
```

### Effect::Side は fire-and-forget

`Effect::Side` で実行される副作用（`SideEffect::execute`）の成否は
`ask()` の呼び出し元には **伝わりません**。

これは意図的な設計です。Decider の主目的はイベントの永続化であり、
副作用の成否によって主フローが止まることを避けるためです。

```rust
// ask() は Effect::Side が失敗しても Ok を返す
let events = proxy.ask(TriggerSomeSideEffect).await; // Ok(vec![])
```

副作用が失敗した場合は `tracing::warn` でログが出力されます。
補償が必要な場合は、別の仕組み（Saga 等）で対処してください（本ライブラリのスコープ外）。

### Effect::Apply と Effect::Persist の違い

| バリアント | EventStore への書き込み | apply() の呼び出し |
|-----------|------------------------|---------------------|
| `Persist(events)` | **あり** | あり |
| `Apply(events)` | **なし** | あり |

`Apply` はテストや一時的な状態変化（UI プレビュー等）に使います。
通常の CQRS+ES では `Persist` を使います。

## Replay / Snapshot の挙動

### Replay

プロセス起動時（`on_start`）に `EventStore` から全イベントを読み込み、
`apply()` を順に呼び出して状態を復元します。

```
on_start:
  1. SnapshotStore から最新スナップショットを取得（Snapshotable の場合）
  2. スナップショット以降のイベントを EventStore から読み込む
  3. Aggregate::apply() を順に呼び出す
  4. メッセージループ開始
```

### なぜ on_start 完了前にメッセージを処理しないか

`on_start` が完了するまでメッセージループは開始しません。
これにより、リプレイ中にコマンドが到着しても、**完全に復元された状態**で処理されます。

```
spawn()  →  on_start (replay) runs to completion
         →  message loop starts
         →  queued messages processed against fully-replayed state
```

### Snapshot

`Snapshotable` を実装することで、リプレイを高速化できます。

1. スナップショット保存のタイミングはアプリケーション側が決定します
2. スナップショットから復元後、スナップショット以降の差分イベントだけを適用します
3. スナップショットがない場合は全イベントを最初から再生します

```rust
// スナップショット保存
snapshot_ref.save(PersistedSnapshot {
    aggregate_id: id.clone(),
    sequence: current_seq,
    payload: codec.encode(&aggregate.capture())?,
    created_at: jiff::Timestamp::now(),
}).await?;
```

## Projection の Catch-up + Live 設計

### Catch-up

Projector 起動時に、チェックポイント以降の全イベントを `EventStore` から読み込んで処理します。
この処理は `on_start` で実行されるため、完了してからライブイベントの処理が始まります。

```
ProjectorProcess::on_start:
  1. CheckpointStore から最後の処理位置を取得
  2. EventStore から (checkpoint+1) 以降のイベントを読み込む
  3. 各イベントを project() で処理
  4. デリバリーモードに従ってチェックポイントを更新
```

### Live

`ProjectorProps::subscribe(stream)` で `Stream<EventEnvelope<E>>` を購読すると、
新しいイベントがリアルタイムに届きます。

**注意**: 現在の実装では、Aggregate が `ask()` で永続化したイベントは
自動的にライブストリームに publish されません。
publish が必要な場合は `Effect::publish(stream, envelope)` を
`decide()` から明示的に返してください。

### Catch-up と Live の順序保証

`on_start` (catch-up) が完了してからメッセージループ (live) が始まります。
これにより、Catch-up と Live の間でイベントが重複しません（at-most-once の範囲で）。

ただし、Catch-up 完了後に Live ストリームに届いたイベントで
Catch-up 期間中のイベントが重複する可能性があります（at-least-once の場合）。
`Projector` は冪等になるよう実装してください。

## DeliveryMode の選び方

| 状況 | 推奨モード |
|------|-----------|
| リードモデル更新が冪等（常に上書き） | `AtLeastOnce` |
| 失敗時に重複処理してほしくない（カウンタ等） | `ExactlyOnce`（ユーザー実装） |
| 最善努力で処理、失敗してもリトライ不要 | `AtMostOnce` |

### ExactlyOnce の実装パターン

ExactlyOnce はフレームワークが提供するのではなく、ユーザーが `project()` 内で
リードモデルとチェックポイントを同一トランザクションで保存することで実現します。

```rust
async fn project(&mut self, event: MyEvent, ctx: &mut ProjectionContext<'_, MyTx>) -> Result<(), Self::Error> {
    // 1. トランザクション内でリードモデルを更新
    // 2. 同じトランザクション内でチェックポイントを保存
    self.checkpoint_store
        .save(&self.projection_id, ctx.current_sequence(), None)
        .await?;
    Ok(())
}
```

## スコープ外の設計判断

### Saga / Process Manager を含まない理由

Saga は複数の集約間にまたがる補償トランザクションを調整します。
これは本ライブラリの現フェーズのスコープを超えています。
Phase 2 では「Send-and-pray」パターン（`Effect::Side`）のみを提供します。

### スキーマ進化を含まない理由

イベントのスキーマが変化した場合の処理（アップキャスト/ダウンキャスト）は
複雑なトレードオフを伴います。将来の TUI ツールで対応予定です。

### 具体的なバックエンドを含まない理由

Postgres、SQLite、DynamoDB など多様なバックエンドへの依存を
このクレートに持ち込むと、依存関係が肥大化します。
各バックエンドはサードパーティ crate として提供することを推奨します。
