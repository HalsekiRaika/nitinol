canonical quality gate script `.takt/quality-gates/rust-quality.sh`自体の欠陥だけを修復してください。

- 監査Findingとログから、製品failureではなくscript defectである根拠を確認する。
- 変更対象を原則として`.takt/quality-gates/rust-quality.sh`に限定する。
- 製品コードやテストの失敗を隠すためにコマンド、deny規則、対象範囲を削除しない。
- `bash -n .takt/quality-gates/rust-quality.sh`を通す。
- 報告された欠陥を確認できる最小の`--scope`実行を行う。
- full gateが別の既存failureで止まる場合、それをこのタスクへ混ぜない。
