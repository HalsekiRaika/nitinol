01-project-consistency-plan.mdに従い、プロジェクト全体を3パートに分けて監査してください。
コードや設定は変更しません。

監査開始時に必ず次を読んでください。

- `.takt/runs/project-consistency-audit/mechanical-check.md`
- `.takt/runs/project-consistency-audit/rust-quality.log`（必要な箇所のみ）
- 正規パス`.takt/quality-gates/rust-quality.sh`

機械検査レポートが存在しない場合は、監査を成功扱いにせず、command gateの実行・設定・権限を
`quality_gate` findingとして調査してください。

## チーム分割

計画の3系統を各partへ割り当てます。各partは、担当範囲をリポジトリ全体検索で棚卸ししてから、
候補を文脈付きで確認してください。単一ファイルの精読だけで全件監査と宣言してはいけません。

Part 1は次の4点を独立に判定します。

1. `.takt/quality-gates/rust-quality.sh`の実行結果そのもの
2. 有効Facetから導かれる機械判定可能な要求
3. スクリプトが実際に実行する検査と対象範囲
4. workflow、`.takt/config.yaml`、CI、Nix/WSL環境への接続

PASSは「現行スクリプトが通った」ことだけを意味します。期待契約とのcoverage差分を必ず確認します。
FAILは、製品欠陥・テスト欠陥・スクリプト欠陥・環境欠陥・配線欠陥に分類してください。

## 統合時の要件

- 重複findingを統合し、異なる根本原因を一つに潰さない。
- findingごとに、分類、重大度、根拠、影響、最小修正方向を示す。
- 分類には`quality_gate`を使用できる。
- 「Facetの内容が悪い」のか「適切なstepへ接続されていない」のかを区別する。
- 「コードが壊れている」のか「検査が古い・不足・過剰」なのかを区別する。
- 現在の挙動と文書が異なる場合、どちらが正かを証拠なしに決めない。
- 既に取得済みの機械検査をLLMが同じコマンドで再実行しない。
- 既存タスクと重複する候補、他候補の完了で消滅する候補を明示する。
- 改善候補は依存グラフと推奨順序を持たせる。
- 人間の決定が必要なものは`decision_required`として分離する。

最終レポートはproject-consistency-audit output contractに従ってください。
