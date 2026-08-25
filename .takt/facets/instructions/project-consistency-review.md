03-project-consistency-supervision.mdで指摘された不足だけを追加監査してください。
コードや設定は変更しません。

## Report Phaseとの境界

- Workflow ContextのReport DirectoryはPhase 1では読み取り専用です。
- `02-project-consistency-audit.md`を直接更新しないでください。
- 追加監査の差分はPhase 1の最終回答として返してください。
- Workflowでは別名のレポート、推奨名`04-project-consistency-audit-review.md`へ保存してください。
- ファイル保存はWorkflowの`output_contracts`とTAKTのReport Phaseが担当します。

- 既存findingを言い換えて件数だけ増やさない。
- 未監査のCriteriaまたはプロジェクト領域を実際に検索、確認する。
- `.takt/`を再帰的に探索しない。例外は正規`rust-quality.sh`と生成済み機械検査証拠だけとする。
- 機械検査の再実行は原則行わず、既存report、raw log、スクリプト内容を照合する。
- 追加実行が不可欠なら、既存証拠では答えられない問いと必要な限定コマンドを明示する。
- PASSをcoverage十分の根拠として扱わない。
- FAILを即座にquality gateスクリプトの欠陥と決めつけない。
- 根拠不足ならfile:line、CriteriaID、または機械検査ログ参照を追加する。
- Criteriaまたは仕様の矛盾やローカル証拠で解けない問題は`decision_required`へ移す。
- 新しいfindingで依存順や優先度が変わる場合は改善候補表を更新する。
- 監督者の各指摘に、解消済み、未解消、方針決定待ちの状態を付ける。
- `.takt/quality-gates/rust-quality.sh`以外のTAKT変更候補を削除する。
