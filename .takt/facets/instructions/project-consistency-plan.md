プロジェクト全体の整合性監査計画を作成してください。実装や設定変更は行いません。
Knowledge `rust-audit-standard`を監査基準として使用します。
Knowledge内のCriteriaは成果物についての評価命題であり、監査者への実装指示ではありません。

## Report Phaseとの境界

- Workflow ContextのReport DirectoryはPhase 1では読み取り専用です。
- `01-project-consistency-plan.md`を直接作成、変更、移動、削除しないでください。
- 計画内容はPhase 1の最終回答として返してください。
- ファイル保存はWorkflowの`output_contracts`とTAKTのReport Phaseが担当します。

このstepの完了後、command quality gateが正規パス
`.takt/quality-gates/rust-quality.sh`を実行し、
`.takt/runs/project-consistency-audit/mechanical-check.md`へ結果を保存します。
計画時点ではスクリプトを重複実行しないでください。

## 1. Audit Standardを確認する

- Source ManifestとStandard Revisionを計画へ記録する。
- Criteria IDを再採番せず、期待特性、判定種別、違反条件の意味を変更しない。
- 現在のリポジトリへ適用可能なCriteriaを選び、適用対象と必要な証拠を具体化する。
- 適用不能なCriteriaも追跡表へ残し、理由を`not_applicable`候補として示す。
- 元PolicyファイルやTAKT Resource Mapを再探索しない。
- Source Manifestと実際のPolicy鮮度が確認できない場合は`unknown`とし、規格をその場で再生成しない。

Criteria間または現行仕様との矛盾は無理に統合せず、`decision_required`候補として記録してください。
Audit StandardやPolicy自体の改善案は作りません。

## 2. 機械検査の期待契約を作る

`mechanical`または`both`のCriteriaと、Cargo、toolchain、Nix、CIのプロジェクト設定から、
機械判定可能な期待契約を作成してください。

静的に次を照合します。

- Criteria ID
- 機械判定可能な内容
- `.takt/quality-gates/rust-quality.sh`内の対応箇所
- Cargo、toolchain、Nix、CIとの関係
- 後続auditで確認すべきcoverage不足候補

一般的なRust慣行だけを理由に検査を追加せず、必ずCriteriaまたはプロジェクト内の根拠へ結び付けます。

## 3. プロジェクトの情報源を棚卸しする

少なくとも次を検索してください。

- README、仕様書、ADR、設計文書、ロードマップ
- Cargo workspace/crate構成、feature、toolchain、Nix/WSL設定
- 公開API、永続化、serialization、protocol、migration契約
- `src/`、`tests/`、examples、生成物
- CI設定、pre-commit、Makefile、justfile、task runner
- git履歴、現在差分、与えられたTAKTタスクキューの要約

`.takt/`を再帰的に棚卸ししてはいけません。読むことができる例外は、正規パス
`.takt/quality-gates/rust-quality.sh`と生成済み機械検査証拠だけです。
情報源が矛盾する場合は、計画時点で勝者を決めず矛盾候補として記録してください。

## 4. 監査を3パートへ分割する

1. **Criteria Compliance / Architecture / Rust Implementation**
2. **Mechanical Checks / Build and Test Infrastructure**
3. **Goal / Spec / Tests / Documentation / Task Drift**

各パートについて対象パス、Criteria ID、確認観点、完了条件を示してください。

## 5. 監査完了条件

- 主要モジュール、公開契約、データ境界が未割当になっていない
- 適用可能なCriteriaと担当Partを追跡できる
- 機械判定可能なCriteriaと`rust-quality.sh`の対応関係を追跡できる
- 機械検査結果を後続監査で原因分類できる
- Findingを`file:line`、Criteria ID、または機械検査ログで裏付けられる
- 改善候補を依存順に並べられる
- 人間の方針決定が必要な項目を実装タスクから分離できる
- `.takt/quality-gates/rust-quality.sh`以外のTAKT管理リソースを監査対象へ含めていない
