01-project-consistency-plan.mdに従い、プロジェクトを3パートに分けて監査してください。
コードや設定は変更しません。

監査開始時に必ず次を読んでください。

- `01-project-consistency-plan.md`のPolicy監査基準表
- `.takt/runs/project-consistency-audit/mechanical-check.md`
- `.takt/runs/project-consistency-audit/rust-quality.log`（必要な箇所のみ）
- 正規パス`.takt/quality-gates/rust-quality.sh`

`.takt/`の他のリソースを探索、評価、改善提案の対象にしてはいけません。
機械検査レポートが存在しない場合は、製品改善findingを作らず、監査インフラの障害として
監査を中止してください。

## チーム分割

計画の3系統を各partへ1つずつ割り当てます。各partは担当範囲をリポジトリ検索で棚卸ししてから、
候補を文脈付きで確認してください。単一ファイルの精読だけで全件監査と宣言してはいけません。

### Part 1: Policy Compliance / Architecture / Rust Implementation

- 計画で割り当てられた`POL-xxx`基準を、設計、責務境界、Rust実装、公開APIへ照合する。
- Policy自体の文面やTAKT上の適用方法は評価しない。
- 違反、準拠、判定不能を基準IDごとに示す。

### Part 2: Mechanical Checks / Build and Test Infrastructure

次を独立に判定します。

1. `.takt/quality-gates/rust-quality.sh`の実行結果そのもの
2. 計画で抽出された機械判定可能なPolicy基準
3. スクリプトが実際に実行する検査と対象範囲
4. Cargo、toolchain、Nix、CIが定義する再現可能な品質環境

PASSは現行スクリプトが通ったことだけを意味します。期待契約とのcoverage差分を確認してください。
FAILは、製品欠陥、テスト欠陥、スクリプト欠陥、プロジェクト環境定義、操作者固有環境へ分類します。

### Part 3: Goal / Spec / Tests / Documentation / Task Drift

- 目標、仕様、公開契約、実際の振る舞い、意味的テストcoverage、文書、既存タスクを照合する。
- 小タスクの積み重ねで残った古い前提、重複概念、局所最適を確認する。
- 既存pending/runningタスクとの重複や、先行候補で消滅する問題を特定する。

## 統合時の要件

- 重複findingを統合し、異なる根本原因を一つに潰さない。
- findingごとに分類、重大度、Policy基準ID、根拠、影響、最小修正方向を示す。
- 「コードまたはテストが壊れている」のか「検査が古い、不足、過剰」なのかを区別する。
- 現在の挙動と文書が異なる場合、どちらが正かを証拠なしに決めない。
- 既に取得済みの機械検査を同じコマンドで再実行しない。
- 既存タスクと重複する候補、他候補の完了で消滅する候補を明示する。
- 改善候補は依存グラフと推奨順序を持たせる。
- Policy矛盾や人間の決定が必要なものは`decision_required`として分離する。
- `.takt/`配下の改善候補は`.takt/quality-gates/rust-quality.sh`だけに限定する。

最終レポートはproject-consistency-audit output contractに従ってください。
