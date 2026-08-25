01-project-consistency-plan.md、02-project-consistency-audit.md、
03-project-consistency-supervision.md、および存在する場合は
04-project-consistency-audit-review.md、
`.takt/runs/project-consistency-audit/mechanical-check.md`を読み、
次に実行すべき改善タスクを1件だけ選んでください。

## Report Phaseとの境界

- Workflow ContextのReport DirectoryはPhase 1では読み取り専用です。
- 既存の監査レポートを直接変更しないでください。
- structured outputはPhase 1の最終回答として返してください。
- 永続化が必要な場合はWorkflowの`output_contracts`とTAKTのReport Phaseが担当します。

現在のTAKTタスクキュー:
{context:collect_queue_context.queue}

## 選定手順

1. 監督結果がタスク選定可能であることを確認する。
2. `ready`候補から、pending/runningタスクと実質的に重複するものを除外する。
3. 依存関係の上流を選び、先行候補で消滅する問題を除外する。
4. quality gate失敗は、製品、テスト、スクリプト、プロジェクト環境定義、操作者固有環境の原因分類に従う。
5. Criteria coverage不足をquality gate変更へ変換する場合、決定的、再現可能、過剰に重くない検査だけを対象にする。
6. `.takt/`配下の変更は`.takt/quality-gates/rust-quality.sh`だけに限定する。
7. Finding IDとCriteria IDをタスク説明へ記載してよいが、source commentへの転記を要求しない。
8. 一件のタスクを一つの主目的に限定する。

## Workflow targetの決定

`workflow_target`は次から一つだけ選びます。

- `default-rust`
- `rust-quality-repair-fmt`
- `rust-quality-repair-clippy`
- `rust-quality-repair-dylint`
- `rust-quality-repair-test`
- `rust-quality-repair-structural`
- `rust-quality-repair-script`
- `none`

複数のquality checkが失敗している場合は、full scriptの実行順
`fmt → clippy → dylint → test → structural`で最初の未解消failureだけを選んでください。
後続failureは同じタスクへ混ぜません。

structured outputには`action`, `workflow_target`, `task_markdown`, `title`, `type`,
`decision_reason_code`, `decision_reason`, `decision_evidence`, `next_scan_condition`,
`scope`, `summary`, `goals`, `acceptance_criteria`, `labels`, `issue`を必ず出力してください。

`task_markdown`は選んだWorkflowへそのまま渡される完全な指示書です。
少なくともタイトル、背景・根拠、対象範囲、明示的な対象外、目標、受け入れ条件、
検証方法を含め、他のstructured fieldと内容を一致させてください。

`enqueue_new_task`の場合:
- `workflow_target`は`none`以外
- `decision_reason_code`は`task_selected`
- `decision_reason`に選定理由を具体的に記載する
- `decision_evidence`に根拠を1件以上記載する
- `next_scan_condition`は空文字
- `task_markdown`は空でない完全なタスク指示書
- `goals`は1件以上
- `acceptance_criteria`は2件以上
- `issue`は`{ "create": false }`

`wait_before_next_scan`の場合:
- `workflow_target`は`none`
- `decision_reason_code`は`task_selected`以外
- `decision_reason`は空にしない
- `decision_evidence`に根拠を記載する
- `next_scan_condition`に次回選定条件を記載する
- `task_markdown`, `title`, `scope`, `summary`は空文字
- `type`は`chore`
- `goals`, `acceptance_criteria`, `labels`は空配列
- `issue`は`{ "create": false }`

`action`の候補:
- `enqueue_new_task`
- `wait_before_next_scan`
