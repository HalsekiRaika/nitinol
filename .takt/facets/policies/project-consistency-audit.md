# Project Consistency Audit Policy

## 責務

このPolicyは、プロジェクト全体の整合性監査と改善タスク選定にだけ適用します。
コード、設定、Facet、workflow、仕様書、テスト、品質ゲートを直接変更してはなりません。
監査結果と次タスクの提案だけを作成してください。

ただし、workflowが実行した機械検査の結果は監査証拠として読み取り、失敗や検査範囲の不足を
独立したfindingとして扱います。

## 有効Facetの扱い

- `.takt/`、`~/.takt/`、Builtinの解決順序を区別し、実際に解決される定義を特定する。
- 同名Facetが存在しても内容を混ぜず、上位レイヤーによる置換として扱う。
- workflowの各stepから参照されているFacetだけを、そのstepの有効ルールとして評価する。
- `rust-coding`、`rust-testing`、`rust-design`など役割限定Policyを監査者自身の一般規則へ昇格させない。
- 役割限定Policyは、対象stepへの接続、内容の責務、他Facetとの競合を監査する対象として読む。
- Persona、Policy、Instruction、Knowledge、Output Contractの責務混在をfindingにする。

## 機械検査の扱い

- 正規の機械検査スクリプトは`.takt/quality-gates/rust-quality.sh`だけとし、別パスの同名ファイルへフォールバックしない。
- `.takt/runs/project-consistency-audit/mechanical-check.md`と、そのraw logを一次証拠として読む。
- `.takt/quality-gates/rust-quality.sh`がPASSでも、現在有効なFacet、Cargo/Nix構成、CI、workflowが要求する
  機械判定可能な規則を覆っているとは限らない。検査項目の対応表を作って確認する。
- `.takt/quality-gates/rust-quality.sh`がFAILの場合、次を混同しない。
  1. 製品コード、テスト、生成物の実際の欠陥
  2. スクリプト自身の欠陥、古いコマンド、誤った対象範囲
  3. NixOS/WSL、toolchain、PATH、権限など実行環境の欠陥
  4. workflowやquality gateへの未接続・誤接続
- MISSING、TIMEOUT、実行不能、ログ不十分もquality_gate findingとして扱う。
- LLMが同じ重いコマンドを再実行して結果を上書きしてはならない。追加実行が必要なら、
  既存ログでは答えられない問いと、追加コマンドの必要性を明示する。
- Facetの自然言語規則を無理に機械化しない。決定的・再現可能・十分に安定した規則だけを
  quality gate候補にする。
- 品質ゲートを強化する場合、常時全組合せを走らせる過剰な検査より、変更検知能力と実行コストの
  バランスを優先する。

## 根拠

- findingには、可能な限り`file:line`、設定キー、または機械検査ログへの参照を付ける。
- 観測事実、根拠からの推論、未解決の疑問を明確に分ける。
- 現在のコード、テスト、仕様、履歴が矛盾する場合、証拠なしにどれかを正と決めない。
- 古い文書やタスクを、更新日時だけで現行仕様と判断しない。内容と参照関係を確認する。
- 外部Web調査は、ローカルの根拠だけでは依存仕様を確定できない場合に限る。

## 監査観点

最低限、次を確認します。

1. **Facet / workflow配線**
   - 欠落参照、未使用Facet、旧名参照、誤った役割への適用
   - project/global/Builtinの意図しない上書き
   - InstructionとPolicy、PolicyとOutput Contractの責務競合
   - session再利用による役割間コンテキスト混入
   - read-only stepへの編集要求、編集stepへの不足Policy

2. **目標・仕様・実装の整合性**
   - 現在のプロジェクト目標、仕様、公開契約、アーキテクチャと実装の不一致
   - 小タスクの積み重ねで生じた局所最適、重複概念、責務境界の崩れ
   - 廃止済み設計を前提とするコード、文書、テスト

3. **Rustコード・テスト・品質保証**
   - `rust-review`の意味的ルール
   - 受け入れ条件とテストの対応漏れ
   - command quality gateが検査する範囲とPolicyが要求する範囲の空白
   - `.takt/quality-gates/rust-quality.sh`、CI、Cargo/Nix設定の対象範囲や前提の不一致
   - 実際の機械検査失敗と、その最小原因
   - gateをLLMレビューで無駄に再実行させる重複

4. **タスク化可能性**
   - finding間の依存関係
   - 既存pending/runningタスクとの重複
   - 人間の方針決定が先に必要か
   - 1つの主目的へ分割できるか

## Findingと改善候補

- すべての違和感をタスクへ変換しない。
- cosmetic、好み、根拠のない将来予測は、明確な価値がなければ候補から除外する。
- 改善候補は、正しさ、契約、データ整合性、実行不能なquality gate、Facet/workflow誤配線、
  検査範囲不足、テスト欠落、保守性の順に優先する。
- quality gate失敗の原因が製品コードなら、スクリプト修正で隠さず製品側の修正候補にする。
- quality gateの検査項目が古い・不足・過剰なら、スクリプトまたは配線の修正候補にする。
- 大規模な「全面整理」ではなく、独立検証できる最小の成果単位へ分割する。
- 既存の無関係なPolicy違反を、現在の最優先タスクへ自動的に混ぜない。
