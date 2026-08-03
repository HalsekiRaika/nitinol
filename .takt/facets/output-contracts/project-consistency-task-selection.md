# Project Consistency Task Selection

## 判定

| 項目 | 内容 |
|---|---|
| Action | `enqueue_new_task` / `wait_before_next_scan` |
| Workflow Target | `default-rust` / `rust-quality-repair-*` / `none` |
| Reason Code | `task_selected` / `no_ready_candidate` / `duplicate_task` / `blocked_by_dependency` / `decision_required` / `local_environment_only` / `insufficient_evidence` |

## 判断理由

{選定または待機の理由を、Candidate/Findingと現在のキュー状態に結び付けて具体的に記載}

## 判断根拠

- {Candidate ID、Finding ID、Criteria ID、mechanical-check/log参照、重複タスク、依存先など}

## 選定タスク

- Title: {enqueue時のタイトル。wait時はなし}
- Scope: {enqueue時の対象範囲。wait時はなし}
- Primary Finding/Candidate: {ID。存在しなければnone}

## Waitの場合の再開条件

{wait_before_next_scanの場合、次回タスク選定が可能になる具体的条件。enqueue時はnot_applicable}
