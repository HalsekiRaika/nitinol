# Project Consistency Audit Policy

## 責務

このPolicyは、現在のプロジェクト成果物を既存のRust向けPolicyに照らして監査し、
次の改善タスクを選定するためだけに適用します。監査中にコード、設定、文書、テスト、
Policy、workflow、品質ゲートを直接変更してはなりません。

同時に適用された`rust-design`、`rust-coding`、`rust-testing`、`rust-review`は、
この監査では実装作業の指示ではなく、プロジェクトを評価する監査基準として解釈します。
計画stepはこれらを簡潔な基準IDへ変換し、後続stepはその基準表を使用してください。

## 監査境界

### 監査基準として読むもの

- このworkflowに適用されたRust向けPolicy
- プロジェクト内の仕様、ADR、設計文書、公開契約
- Cargo、toolchain、Nix、CIなど、成果物の再現性と品質に関わる設定

### 監査対象

- プロジェクトの目標、仕様、設計、アーキテクチャ
- Rustコード、公開API、データ形式、protocol、migration
- テスト、examples、生成物、利用者向け文書
- 既存pending/runningタスクとの重複と依存関係
- `.takt/quality-gates/rust-quality.sh`の内容、実行結果、検査coverage

### 監査対象外

`.takt/quality-gates/rust-quality.sh`を除くTAKT管理リソースは監査対象外です。
次を棚卸し、評価、整理、修正提案の対象にしてはいけません。

- `.takt/workflows/`
- `.takt/facets/`のPersona、Policy、Instruction、Knowledge、Output Contract
- `.takt/config.yaml`およびprovider routing
- session、team leader、loop、effectなどworkflow内部構成
- Builtin、global、project resourceの解決順序や上書き関係
- `.takt/quality-gates/project-consistency-machine-check.sh`
- 未使用TAKT Resource、命名、配置、書式

TAKT管理リソースに問題があり監査を実行できない場合は、製品改善findingへ変換せず、
監査インフラの障害としてABORTしてください。`takt workflow doctor`が検出できる問題を、
LLM監査で重複して探してはいけません。

## Policyの扱い

- TAKTがこのstepへ解決・適用したPolicy内容を監査基準として使用する。
- resource map、解決元、上書き一覧、未使用Policy一覧を作成しない。
- Policyごとに、評価可能な要求を短い基準IDへ変換する。
- 基準は`semantic`、`mechanical`、`both`へ分類する。
- 役割限定の文言は、対象成果物に適用できる要求だけを抽出する。
- Policy同士が矛盾する、適用範囲が不明、または現在の仕様と両立しない場合は、
  どちらかを勝手に正とせず`decision_required`へ分離する。
- Policy自体の文面改善やFacet配線変更を、`default-rust`タスクへ積まない。

## 機械検査の扱い

- 正規の機械検査スクリプトは`.takt/quality-gates/rust-quality.sh`だけとし、
  別パスの同名ファイルへフォールバックしない。
- `.takt/runs/project-consistency-audit/mechanical-check.md`とraw logを一次証拠として読む。
- PASSは現在のスクリプトが成功したことだけを意味し、Policy由来の機械判定可能要件を
  十分に覆っていることの証明にはしない。
- FAILは次を混同せず分類する。
  1. 製品コード、テスト、生成物の欠陥
  2. スクリプト自身の欠陥、古いコマンド、誤った対象範囲
  3. Cargo、toolchain、Nix、CIなどプロジェクトで再現可能な環境定義の欠陥
  4. 操作者固有の一時的な環境問題
- MISSING、TIMEOUT、実行不能、ログ不十分も機械検査findingとして扱う。
- LLMが同じ重いコマンドを再実行して結果を上書きしてはならない。
- 自然言語規則を無理に機械化しない。決定的、再現可能、安定的な規則だけを
  quality gate候補にする。
- 検査強化では、変更検知能力と実行コストのバランスを優先する。

## 根拠

- findingには可能な限り`file:line`、Policy基準ID、または機械検査ログ参照を付ける。
- 観測事実、根拠からの推論、未解決の疑問を明確に分ける。
- コード、テスト、仕様、履歴が矛盾する場合、証拠なしにどれかを正と決めない。
- 更新日時だけで現行仕様を判断せず、内容と参照関係を確認する。
- 外部Web調査は、ローカル根拠だけでは外部依存の仕様を確定できない場合に限る。

## 監査観点

最低限、次を確認します。

1. **Policy準拠**
   - 設計、実装、テストが抽出済みPolicy基準を満たすか
   - 局所的な小タスクの積み重ねで、責務境界や不変条件が崩れていないか
   - Policyで要求される検証がコードやテストで実証されているか

2. **目標・仕様・実装の整合性**
   - プロジェクト目標、仕様、公開契約、アーキテクチャと実装の不一致
   - 廃止済み設計を前提とするコード、文書、テスト
   - 重複概念、責務の逆転、互換性やデータ整合性の破壊

3. **Rustコード・テスト・品質保証**
   - 意味的レビューでのみ判断できるRust上の問題
   - 受け入れ条件、公開契約、失敗モードとテストの対応漏れ
   - Policyの機械判定可能要件と`rust-quality.sh`のcoverage差分
   - `rust-quality.sh`、Cargo、toolchain、Nix、CIの対象範囲や前提の不一致
   - 実際の機械検査失敗と、その最小原因

4. **タスク化可能性**
   - finding間の依存関係
   - 既存pending/runningタスクとの重複
   - 人間の方針決定が先に必要か
   - 1つの主目的へ分割できるか

## Findingと改善候補

- すべての違和感をタスクへ変換しない。
- cosmetic、好み、根拠のない将来予測は、明確な価値がなければ候補から除外する。
- 改善候補は、正しさ、公開契約、データ整合性、Policy違反、実行不能な機械検査、
  検査coverage不足、テスト欠落、保守性の順に優先する。
- 機械検査失敗の原因が製品コードなら、スクリプト修正で隠さず製品側を修正する。
- 検査項目が古い、不足、過剰なら、`rust-quality.sh`の修正候補にする。
- `.takt/`配下の改善候補は`.takt/quality-gates/rust-quality.sh`だけに限定する。
- 大規模な「全面整理」ではなく、独立検証できる最小成果単位へ分割する。
- 既存の無関係なPolicy違反を、最優先タスクへ自動的に混ぜない。
