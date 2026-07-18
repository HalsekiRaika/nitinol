# Rust Quality Recovery Policy

## 目的

このPolicyは、既に失敗しているRust quality gateを一項目ずつ回復させる専用Workflowに適用します。
通常開発のfull gateを置き換えたり、品質基準を弱めたりするためには使用しません。

## 修復境界

- タスクとWorkflowが指定した対象検査だけをblocking gateとして扱います。
- 対象外の既存failureは、このタスクの失敗理由にしません。
- 対象外failureを同時に修正してタスク範囲を広げません。
- full `rust-quality.sh`が対象検査より後で失敗しても、対象検査が通り、変更が妥当なら修復タスクは完了可能です。
- 次の残存failureは、次回Auditで独立Findingとして扱います。

## 変更規則

- Finding、機械検査ログ、タスク記載の根拠に結び付く最小変更だけを行います。
- 警告抑制、検査除外、テスト削除、assertion弱体化によって見かけ上PASSさせません。
- lint allowや設定緩和は、規則自体が誤っている明確な根拠がある場合に限ります。
- 公開API、データ形式、互換性、責務境界へ影響する場合は、機械検査PASSだけで妥当と判断しません。
- `.takt/`を変更できるのは、タスクがquality script修復を明示した場合の
  `.takt/quality-gates/rust-quality.sh`だけです。

## 対象別規則

- `fmt`: `cargo fmt --all`による整形だけを行い、意味的変更を加えません。
- `clippy`: lintの原因を修正し、安易なallow追加や契約変更を避けます。
- `dylint`: project policy由来のlint意図を保ち、検査そのものを無効化しません。
- `test`: 製品欠陥とテスト欠陥を区別し、正しい契約に合わせて最小側を修正します。
- `structural`: module path、公開API、文書リンクへの影響を確認します。
- `script`: canonical scriptの欠陥だけを修正し、失敗している製品検査を隠しません。

## 完了条件

- Workflowに設定された限定command gateが成功する。
- 変更が対象検査の根本原因へ対応している。
- 対象外の品質基準を弱めていない。
- 残存するfull gate failureを、このタスクで解消済みと表現しない。
