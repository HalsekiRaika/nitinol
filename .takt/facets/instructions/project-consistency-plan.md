プロジェクト全体の整合性監査計画を作成してください。実装や設定変更は行いません。
このstepの完了後、command quality gateが正規パス`.takt/quality-gates/rust-quality.sh`を実行して
`.takt/runs/project-consistency-audit/mechanical-check.md`へ結果を保存します。
計画時点ではスクリプトを重複実行せず、静的な内容と接続だけを確認してください。

## 1. 現在有効なTAKT構成を確定する

次を棚卸しし、workflowの各stepが最終的に参照するFacetを表にしてください。

- `.takt/workflows/**/*.yaml`
- `.takt/facets/{personas,policies,instructions,knowledge,output-contracts}/**/*`
- `.takt/config.yaml`とworkflow override
- `~/.takt/`が読める場合は同名resourceの存在
- Builtinを上書きしているproject-local resource
- 正規パス`.takt/quality-gates/rust-quality.sh`と、その適用step
- `workflow_command_gates.custom_scripts`の有効化状態

ファイル名だけで同一内容と判断せず、置換されたBuiltinとの差分と責務を確認してください。

## 2. 機械検査の期待契約を作る

現在有効なFacet、workflow、Cargo/Nix設定、CIから、機械判定可能な要求を抽出してください。
この段階では「一般的なRustプロジェクトなら実行すべき」という理由だけで追加せず、
プロジェクト内の根拠と結び付けます。

最低限、次を表にします。

- 要求元Facetまたは設定
- 機械判定可能な内容
- `.takt/quality-gates/rust-quality.sh`内の対応箇所
- workflow上の実行タイミング
- CIとの重複または差異
- 監査時に確認すべき不足候補

`.takt/quality-gates/rust-quality.sh`のPASS/FAIL結果は後続audit stepで読みます。

## 3. プロジェクトの正とされる情報源を棚卸しする

少なくとも次を検索してください。

- README、仕様書、ADR、設計文書、ロードマップ
- Cargo workspace/crate構成、feature、toolchain、Nix/WSL設定
- 公開API、永続化・serialization・protocol契約
- `src/`、`tests/`、examples、migration、生成物
- CI設定、pre-commit、Makefile、justfile、task runner
- git履歴、現在差分、TAKTのtask/order/reportが存在すればその要約

情報源が矛盾する場合は、監査計画の時点で勝者を決めず、矛盾として記録してください。

## 4. 監査を3パートへ分割する

重複を最小化し、全対象を次の3系統へ割り当ててください。

1. TAKT構成・Facet・workflow・quality gateの有効性、および機械検査結果
2. 目標・仕様・アーキテクチャ・公開契約と実装の整合性
3. Rustコード・テスト・タスク履歴・文書の横断整合性

各パートについて、対象ディレクトリ、確認観点、完了条件を示してください。

## 5. 監査完了条件

- 主要モジュールと重要契約が未割当になっていない
- 各Facetの責務と適用stepを追跡できる
- 機械判定可能なFacet要求と`.takt/quality-gates/rust-quality.sh`の対応関係を追跡できる
- 機械検査の実行結果を後続監査で分類できる
- findingをfile:line、設定キー、または機械検査ログで裏付けられる
- 改善候補を依存順に並べられる
- 人間の方針決定が必要な項目を、実装タスクから分離できる
