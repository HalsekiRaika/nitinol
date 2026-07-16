プロジェクト全体の整合性監査計画を作成してください。実装や設定変更は行いません。
このstepに適用された`rust-design`、`rust-coding`、`rust-testing`、`rust-review`を
監査基準として使用します。

このstepの完了後、command quality gateが正規パス
`.takt/quality-gates/rust-quality.sh`を実行し、
`.takt/runs/project-consistency-audit/mechanical-check.md`へ結果を保存します。
計画時点ではスクリプトを重複実行しないでください。

## 1. Policy監査基準を抽出する

適用済みPolicyから、現在の成果物を評価できる要求だけを簡潔な基準へ変換してください。
TAKT Resource Mapや解決元一覧は作成しません。

各基準には次を含めます。

- `POL-xxx`形式の一意なID
- Policy名
- 要求の簡潔な要約
- 適用対象（設計、実装、API、エラー処理、テストなど）
- 判定種別（semantic / mechanical / both）
- 準拠を確認するために必要な証拠

同じ要求は統合し、Policy間で矛盾や適用範囲の曖昧さがある場合は、基準へ無理に統合せず
`decision_required`候補として記録してください。Policyファイル自体の改善案は作りません。

## 2. 機械検査の期待契約を作る

`mechanical`または`both`に分類したPolicy基準と、Cargo、toolchain、Nix、CIの
プロジェクト設定から、機械判定可能な期待契約を作成してください。

静的に次を照合します。

- Policy基準ID
- 機械判定可能な内容
- `.takt/quality-gates/rust-quality.sh`内の対応箇所
- Cargo、toolchain、Nix、CIとの関係
- 後続auditで確認すべきcoverage不足候補

一般的なRust慣行だけを理由に検査を追加せず、必ずPolicyまたはプロジェクト内の根拠へ結び付けます。

## 3. プロジェクトの情報源を棚卸しする

少なくとも次を検索してください。

- README、仕様書、ADR、設計文書、ロードマップ
- Cargo workspace/crate構成、feature、toolchain、Nix/WSL設定
- 公開API、永続化、serialization、protocol、migration契約
- `src/`、`tests/`、examples、生成物
- CI設定、pre-commit、Makefile、justfile、task runner
- git履歴、現在差分、与えられたTAKTタスクキューの要約

`.takt/`を再帰的に棚卸ししてはいけません。読むことができる例外は、監査基準として
既に適用されたPolicy内容、正規パス`.takt/quality-gates/rust-quality.sh`、および
生成済み機械検査証拠だけです。

情報源が矛盾する場合は、計画時点で勝者を決めず矛盾候補として記録してください。

## 4. 監査を3パートへ分割する

重複を最小化し、次の3系統へ割り当ててください。

1. **Policy Compliance / Architecture / Rust Implementation**
   - Policy基準と設計、アーキテクチャ、Rust実装、公開APIの準拠
2. **Mechanical Checks / Build and Test Infrastructure**
   - `rust-quality.sh`の実行結果、検査coverage、Cargo、toolchain、Nix、CIとの整合
3. **Goal / Spec / Tests / Documentation / Task Drift**
   - 目標、仕様、振る舞い、意味的テストcoverage、文書、既存タスク間のずれ

Part 2の「テスト」はコマンド、対象範囲、実行環境を扱い、Part 3は仕様や失敗モードに対する
テスト内容の十分性を扱います。

各パートについて対象パス、Policy基準ID、確認観点、完了条件を示してください。

## 5. 監査完了条件

- 主要モジュール、公開契約、データ境界が未割当になっていない
- Policy要求が基準IDへ変換され、各基準の担当Partを追跡できる
- 機械判定可能なPolicy基準と`rust-quality.sh`の対応関係を追跡できる
- 機械検査結果を後続監査で原因分類できる
- findingをfile:line、Policy基準ID、または機械検査ログで裏付けられる
- 改善候補を依存順に並べられる
- 人間の方針決定が必要な項目を実装タスクから分離できる
- `.takt/quality-gates/rust-quality.sh`以外のTAKT管理リソースを監査対象へ含めていない
