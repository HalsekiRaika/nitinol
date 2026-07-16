# Project Consistency Audit Plan

## Audit Standard

- Resource: `rust-audit-standard`
- Standard Revision: {revision}
- Source Manifest Status: current / unknown / drift
- 備考: {Source Policy更新の有無、確認不能理由}

## 監査対象の概要

{リポジトリ、workspace、主要crate、現在のブランチ・差分の概要}

## Criteria監査基準

| Criteria ID | Source Policy / Clause | 規範主体 | 期待される成果物特性 | 適用対象 | 判定種別 | 必要な証拠 | 違反条件 |
|---|---|---|---|---|---|---|---|
| POL-... | rust-...:line | ... | ... | design / code / API / tests | semantic / mechanical / both | ... | ... |

## Criteria上の曖昧さ・矛盾

| ID | 関係するCriteria・情報源 | 曖昧さ・矛盾 | 監査への影響 | 必要な決定 |
|---|---|---|---|---|
| PD-001 | POL-... | ... | ... | ... |

## 機械検査期待契約

| Contract ID | Criteria | 機械判定可能な要求 | `rust-quality.sh`対応箇所 | Cargo / Nix / CIとの関係 | 監査時の確認 |
|---|---|---|---|---|---|
| MC-001 | POL-... | ... | `script:line` / none | same / partial / none | ... |

## プロジェクト情報源

| 種別 | 場所 | 対象契約 | 鮮度・信頼性 | 矛盾候補 |
|---|---|---|---|---|
| ... | ... | ... | ... | ... |

## 監査パート

### Part 1: Criteria Compliance / Architecture / Rust Implementation
- 対象:
- Criteria ID:
- 観点:
- 完了条件:

### Part 2: Mechanical Checks / Build and Test Infrastructure
- 対象:
- Criteria ID・Contract ID:
- 観点:
- 完了条件:

### Part 3: Goal / Spec / Tests / Documentation / Task Drift
- 対象:
- Criteria ID:
- 観点:
- 完了条件:

## Criteria割当一覧

| Criteria ID | 適用状態 | 担当Part | 対象パス | 確認予定 | 備考 |
|---|---|---|---|---|---|
| POL-... | applicable / not_applicable / unknown | 1 / 2 / 3 | ... | ... | ... |

## 監査対象一覧

| 領域 | 対象パス | 担当Part | 関連Criteria | 状態 |
|---|---|---|---|---|
| ... | ... | ... | POL-... | planned |

## 明示的な対象外

- `.takt/quality-gates/rust-quality.sh`以外のTAKT管理リソース
- TAKT Resource Map、resource解決順序、workflow配線、provider routing
- Audit Standardおよび元Policy自体の文面改善

## 既知の制約・未解決事項

- ...
