# Project Consistency Audit Plan

## 監査対象の概要

{リポジトリ、workspace、主要crate、現在のブランチ・差分の概要}

## Policy監査基準

| Criteria ID | Policy | 要求 | 適用対象 | 判定種別 | 必要な証拠 |
|---|---|---|---|---|---|
| POL-001 | rust-... | ... | design / code / API / tests | semantic / mechanical / both | ... |

## Policy上の曖昧さ・矛盾

| ID | 関係する基準・Policy | 曖昧さ・矛盾 | 監査への影響 | 必要な決定 |
|---|---|---|---|---|
| PD-001 | POL-... | ... | ... | ... |

## 機械検査期待契約

| Contract ID | Policy基準 | 機械判定可能な要求 | `rust-quality.sh`対応箇所 | Cargo / Nix / CIとの関係 | 監査時の確認 |
|---|---|---|---|---|---|
| MC-001 | POL-... | ... | `script:line` / none | same / partial / none | ... |

## プロジェクト情報源

| 種別 | 場所 | 対象契約 | 鮮度・信頼性 | 矛盾候補 |
|---|---|---|---|---|
| ... | ... | ... | ... | ... |

## 監査パート

### Part 1: Policy Compliance / Architecture / Rust Implementation
- 対象:
- Policy基準ID:
- 観点:
- 完了条件:

### Part 2: Mechanical Checks / Build and Test Infrastructure
- 対象:
- Policy基準ID・Contract ID:
- 観点:
- 完了条件:

### Part 3: Goal / Spec / Tests / Documentation / Task Drift
- 対象:
- Policy基準ID:
- 観点:
- 完了条件:

## 監査対象一覧

| 領域 | 対象パス | 担当Part | 関連基準 | 状態 |
|---|---|---|---|---|
| ... | ... | ... | POL-... | planned |

## 明示的な対象外

- `.takt/quality-gates/rust-quality.sh`以外のTAKT管理リソース
- TAKT Resource Map、resource解決順序、workflow配線、provider routing
- Policy自体の文面改善

## 既知の制約・未解決事項

- ...
