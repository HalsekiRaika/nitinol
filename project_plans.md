## nitinol-runtime (process層) Phase 1 最終設計まとめ

---

### 設計思想

| 項目 | 方針 |
|------|------|
| レイヤー位置 | 最下層。tokioのみ依存。es/clusterを知らない |
| Actor モデル | Virtual Actor的フラット構造（階層なし） |
| 理由 | DDDの集約間関係はフラットであり、階層的Supervisionはインフラの関心事をドメイン構造に混ぜる |

---

### Props

| 項目 | 決定内容 |
|------|---------|
| パターン | `&mut self` ビルダー |
| 最小フィールド | producer（ファクトリ）+ supervision_strategy |
| 使用 | **強制**（実体渡しAPIなし） |
| 再起動時 | Propsのproducerで新インスタンス生成。Props自体は再利用 |
| リプレイ | process層には含めない（es層が独自の仕組みでPropsを構築して渡す） |

---

### Supervision

| 項目 | 決定内容 |
|------|---------|
| 構造 | フラット（親子階層なし） |
| Supervisorの主体 | ProcessSystem（ランタイム）がProps内の戦略を参照して適用 |
| Directive | **Restart / Stop** の2つのみ |
| 戦略 | OneForOneのみ |
| 障害検知 | `Result`ベース（panicは使わない） |
| 再起動回数制限 | あり（カウンタ+期間方式）。超過時はStop |
| デフォルト閾値 | 未決定（ProtoActor準拠の10回/10秒を参考） |

---

### Watch

| 項目 | 決定内容 |
|------|---------|
| API | `watch(pid)` / `unwatch(pid)` |
| 対象 | 任意のプロセス（親子関係不要） |
| 通知 | `Terminated { who: Pid, why: TerminatedReason }` |
| TerminatedReason | `Stopped` / `NotFound`（`AddressTerminated`はcluster層で将来追加） |
| 既停止時の保証 | DeadLetterのSubscriberがWatch検知 → `Terminated{NotFound}`を返送 |
| 実装 | 被監視側がwatchersセットを保持、停止時に全watcherに通知 |

---

### DeadLetter

| 項目 | 決定内容 |
|------|---------|
| 実装 | 単独ビルトインプロセス |
| 登録 | ProcessSystem自動登録 |
| 用途 | デバッグ |
| 配送条件 | 宛先不在、停止済みプロセス、無効PID |
| Watch連携 | DeadLetterのSubscriberがWatchメッセージを検知 → `Terminated{NotFound}`を返送 |
| Ask時 | DeadLetterResponse自動返送 |
| 中身 | 調整可能（宛先、元メッセージ、送信元） |
| ログ抑制 | マーカートレイトで制御（通知は止めない、ログのみ抑制） |
| ログスロットル | カウンタ+期間でログ爆発防止 |

---

### EventStream

| 項目 | 決定内容 |
|------|---------|
| 配置 | process層 |
| Stream | ビルトインプロセスとして動作 |
| 型パラメータ | `Stream<T = Boxed>`。デフォルト=何でもpublish可、型指定=コンパイル時チェック |
| Publish | Streamプロセスへメッセージ送信。登録不要 |
| Publish API | `ProcessProxy<Stream<Boxed>>` は `publish(impl Message + Clone)` を持つ（条件付きimpl） |
| Subscribe | `Subscriber<T>`トレイト実装 → Props構築 → spawn → 購読プロセスとして登録 |
| `Subscriber<T>`のT | メッセージ型（process層にEvent概念はない） |
| 発見 | 他プロセスと同様にlookup |
| スコープ | ProcessSystemにユーザーがStreamを配置（Topic単位でユニーク） |
| DeadLetter Stream | ProcessSystem自動登録 |
| グローバルStream | ユーザーが任意に定義可能 |

---

### Phase 1 完了条件

process層単体でCQRS+ESに必要なプロセスモデル機能が動くこと：
- Props経由のプロセス生成（強制）
- Tell/Ask メッセージ送受信
- ライフサイクル（start/stop/poison）
- ProcessRegistry lookup
- Supervision（Restart/Stop、OFO、回数制限）
- Watch/Unwatch → Terminated通知
- DeadLetter（自動登録、配送、Watch連携、Ask返送）
- EventStream（Streamプロセス、Pub/Sub）
- 全機能のテスト

---

### 未決定事項

| 項目 | 状態 |
|------|------|
| Supervision デフォルト閾値 | 未決定（10回/10秒を参考） |
| Props拡張フィールド（middleware, onInit等） | 将来Phase |
| ExponentialBackoff戦略 | 将来Phase |
| AddressTerminated | cluster層で将来追加 |

---

### 前回の壁打ちからの変更点

| 項目 | 前回 | 今回 |
|------|------|------|
| Props | 不要とされていた | **必須**。ファクトリ+Supervision用。強制使用 |
| DeadLetter | Phase 2予定 | **Phase 1に前倒し** |
| EventStream | 明示的な議論なし | **process層のビルトインプロセスとして設計** |
| Supervision | Phase 2予定 | **Phase 1でRestart/Stop + OFOを導入** |
| Watch | 未議論 | **Phase 1で導入。ProtoActor方式** |
| Phase 1スコープ | ES使い勝手＋Decider＋InMemoryEventStore | **process層単体の完成**に絞り込み |
| 構造 | 曖昧 | **フラット構造（Virtual Actor的）を明示** |
