# Project Consistency Audit Control Policy

## 責務

このPolicyは、現在のプロジェクト成果物を監査するAgentの行動だけを制御します。
コード、設定、文書、テスト、Policy、workflow、品質ゲートを監査中に直接変更してはなりません。
監査基準はKnowledge `rust-audit-standard`および計画レポート内のCriteria表から取得します。

## 監査境界

監査対象は次です。

- プロジェクトの目標、仕様、設計、アーキテクチャ
- Rustコード、公開API、データ形式、protocol、migration
- テスト、examples、生成物、利用者向け文書
- Cargo、toolchain、Nix、CIなどの再現可能な品質環境
- 既存pending/runningタスクとの重複と依存関係
- `.takt/quality-gates/rust-quality.sh`の内容、実行結果、検査coverage

`.takt/quality-gates/rust-quality.sh`を除くTAKT管理リソースは監査対象外です。
次を棚卸し、評価、整理、Finding、改善候補の対象にしてはいけません。

- `.takt/workflows/`
- `.takt/facets/`
- `.takt/config.yaml`およびprovider routing
- session、team leader、loop、effectなどworkflow内部構成
- Builtin、global、project resourceの解決順序
- `.takt/quality-gates/project-consistency-machine-check.sh`

TAKT管理リソースの問題で監査を実行できない場合は、製品Findingへ変換せず監査インフラ障害としてABORTします。

## 監査基準の扱い

- Criteria ID、Source Policy、期待される成果物特性、違反条件の意味を勝手に変更しません。
- Criteria内の表現は成果物の規範であり、監査Agent自身への実装指示ではありません。
- 適用不能は`not_applicable`、証拠不足は`unknown`として扱い、違反へ推測変換しません。
- 基準同士または現行仕様との矛盾は、勝者を決めず`decision_required`へ分離します。
- PolicyやAudit Standard自体の改善を製品タスクへ混入させません。

## 機械検査

- 正規スクリプトは`.takt/quality-gates/rust-quality.sh`だけです。
- `.takt/runs/project-consistency-audit/mechanical-check.md`とraw logを一次証拠として読みます。
- PASSは現行スクリプトの成功だけを意味し、Criteria coverageの十分性を証明しません。
- FAIL、MISSING、NOT_RUN、TIMEOUTは、製品、テスト、スクリプト、プロジェクト環境定義、操作者固有環境へ分類します。
- 同じ重いコマンドを再実行して既存証拠を上書きしません。
- 自然言語規則を無理に機械化せず、決定的、再現可能、安定的な規則だけをquality gate候補にします。

## 根拠とFinding

- FindingにはCriteria IDと、可能な限り`file:line`または機械検査ログ参照を付けます。
- 観測事実、推論、未解決の疑問を分離します。
- 情報源が矛盾する場合、証拠なしに正しい側を決めません。
- cosmetic、好み、根拠のない将来予測をタスク候補へ変換しません。
- 改善候補は独立検証できる最小成果単位とし、依存関係と既存タスク重複を示します。
- `.takt/`配下の改善候補は`.takt/quality-gates/rust-quality.sh`だけに限定します。
