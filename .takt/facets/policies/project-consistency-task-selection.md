# Project Consistency Task Selection Policy

## 入力境界

- `01-project-consistency-plan.md`、`02-project-consistency-audit.md`、`03-project-consistency-supervision.md`、機械検査証拠、現在のタスクキューだけを選定根拠とします。
- 監督結果がタスク選定可能とした確定Findingおよび`ready`候補だけを選びます。
- 選定stepで新しいFinding、Criteria、設計方針を作りません。

## 選定規則

- pending/runningタスクと実質的に重複する候補を除外します。
- 依存関係の上流を優先し、先行候補で消滅する問題を選びません。
- 正しさ、公開契約、データ整合性、明確なCriteria drift、再現可能な品質検査の実行不能を優先します。
- `decision_required`、根拠不足、操作者固有の一時的問題をタスク化しません。
- 単なる再監査、再実行、全面整理を主目的にしません。
- `.takt/`の変更は`.takt/quality-gates/rust-quality.sh`に限定します。
- 1件のタスクは1つの主目的とし、変更対象、非対象、期待成果、検証方法を明示します。
- 独立して実行できる高価値候補がなければ`wait_before_next_scan`を返します。

## 実行レーン

- 通常の機能、設計、実装、テスト、文書改善は`default-rust`へ送ります。
- 現在赤くなっているquality checkそのものを緑へ戻すことが主目的なら、対象検査専用の
  `rust-quality-repair-*`へ送ります。
- quality recoveryでは、一件のタスクに一つの対象検査だけを割り当てます。
- 複数検査が失敗している場合、canonical実行順の最初の未解消failureを優先します:
  `fmt → clippy → dylint → test → structural`。
- canonical script自体の欠陥だけは`rust-quality-repair-script`へ送ります。
- full gateの後続failureが残ることを理由に、対象検査の修復タスクを`default-rust`へ送ってはいけません。
