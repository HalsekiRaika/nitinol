# Project Consistency Audit

## 結論

{現在の整合性状態、機械検査状態、および最も重要な根本原因を短く要約}

## 監査カバレッジ

| Part | 対象 | 確認済み | 未確認 | 備考 |
|---|---|---|---|---|
| 1 | TAKT / Facet / Workflow / Mechanical Checks | ... | ... | ... |
| 2 | Goal / Spec / Architecture / Contract | ... | ... | ... |
| 3 | Rust / Tests / Tasks / Documentation | ... | ... | ... |

## Effective Facet Mapの検証結果

| Workflow / Step | 実効Facet | 解決元 | 適用妥当性 | 問題 |
|---|---|---|---|---|
| ... | ... | ... | OK / DRIFT / UNKNOWN | ... |

## Mechanical Check Execution

| 項目 | 結果 | 根拠 | 解釈 |
|---|---|---|---|
| 正規パスの状態 | PRESENT / MISSING | mechanical-check.mdまたは`file:line` | ... |
| 実行状態 | PASS / FAIL / TIMEOUT_OR_TERMINATED / NOT_RUN | mechanical-check.md / raw log | ... |
| workflow接続 | OK / DRIFT / UNKNOWN | workflow/config key | ... |
| 環境再現性 | OK / DRIFT / UNKNOWN | toolchain/Nix/WSL evidence | ... |

## Mechanical Check Coverage

| Contract ID | Facet・設定の要求 | 現在の検査 | 実行証拠 | Coverage | Gap / 重複 | 推奨方向 |
|---|---|---|---|---|---|---|
| MC-001 | ... | ... | log / script:line | full / partial / none / unknown | ... | ... |

## Findings

| ID | 分類 | 重大度 | 根拠 | 観測事実 | 影響 | 最小修正方向 | 状態 |
|---|---|---|---|---|---|---|---|
| PC-001 | facet_wiring / quality_gate / spec_drift / architecture / tests / docs / task_drift | high / medium / low | `file:line` / mechanical log | ... | ... | ... | confirmed / inferred / unresolved |

## Quality Gate Failure Classification

| Finding | 原因分類 | 製品コード | テスト | スクリプト | 環境 | 配線 | 根拠 |
|---|---|---|---|---|---|---|---|
| PC-... | product / test / script / environment / wiring / mixed / unknown | yes/no | yes/no | yes/no | yes/no | yes/no | ... |

## 矛盾している情報源

| ID | 情報源A | 情報源B | 矛盾 | 解決に必要な根拠 |
|---|---|---|---|---|
| ... | ... | ... | ... | ... |

## Decision Required

| ID | 決定事項 | 選択肢 | 影響範囲 | 実装前に必要な理由 |
|---|---|---|---|---|
| ... | ... | ... | ... | ... |

## 改善候補と依存関係

| Candidate | 対応Finding | 主目的 | 依存先 | 優先度 | 検証方法 | タスク化 |
|---|---|---|---|---|---|---|
| C-001 | PC-... | ... | none / C-... / decision | P0 / P1 / P2 | rust-quality.shまたは限定コマンド | ready / duplicate / blocked / omit |

## 推奨実行順

1. C-...
2. C-...

## 除外した改善

| 項目 | 除外理由 |
|---|---|
| ... | cosmetic / 根拠不足 / 既存タスクと重複 / 他候補で解消 / 機械化不適切 |
