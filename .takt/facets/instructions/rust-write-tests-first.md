{extends:write-tests-first}

## Rust固有のテスト作成手順

1. 計画レポート、対象crateのmanifest、既存テストを確認する。
2. 受け入れ条件ごとに最も狭い層へテストを置く。
   - 純粋な変換・不変条件: unit test
   - 公開crate APIや複数moduleの配線: integration test
   - 公開利用契約: doc test
   - featureや型境界: 必要な場合だけcompile test
3. 正常系、error、境界値、状態遷移、重複、順序の影響を検証する。
4. async testでは固定sleepの延長を使わず、既存の決定的な同期方法を使う。
5. focused testを実行し、Redの原因が未実装要件であることを確認する。
   import、fixture、feature指定、テストlogicの誤りはこのstepで解消する。
6. 未実装APIによりコンパイル不能なら、計画上のsignatureと解消条件を報告する。

プロダクションコードを変更してGreenにしてはいけません。
