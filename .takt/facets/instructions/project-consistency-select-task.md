01-project-consistency-plan.md、02-project-consistency-audit.md、
03-project-consistency-supervision.md、および
`.takt/runs/project-consistency-audit/mechanical-check.md`を読み、
次に実行すべき改善タスクを1件だけ選んでください。

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
8. 通常の`default-rust` workflowが追加設計なしで処理できる粒度にする。

structured outputには`action`, `title`, `type`, `scope`, `summary`, `goals`,
`acceptance_criteria`, `labels`, `issue`を必ず出力してください。

`enqueue_new_task`の場合:
- `goals`は1件以上
- `acceptance_criteria`は2件以上
- `acceptance_criteria`に該当する機械検査または限定検証の実行方法を含める
- `issue`は`{ "create": false }`

`wait_before_next_scan`の場合:
- `title`, `scope`, `summary`は空文字
- `type`は`chore`
- `goals`, `acceptance_criteria`, `labels`は空配列
- `issue`は`{ "create": false }`

`action`の候補:
- `enqueue_new_task`
- `wait_before_next_scan`
