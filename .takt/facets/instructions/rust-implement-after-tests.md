{extends:implement-after-tests}

## Rust固有の実装手順

1. 計画・テストレポートと実際のテストを一次情報として読む。
2. 同種実装、error型、module構成、async runtime、公開API慣習を確認する。
3. テストを満たす最小の一貫した変更を実装する。
   - テスト専用経路や場当たり的分岐を作らない
   - 将来用途だけのtrait、generic、field、fallbackを追加しない
   - Policyを満たすためだけのNewTypeや単一用途traitを作らない
4. テスト変更は、要件または既存契約との明確な矛盾がある場合だけ許容し、
   根拠をDecisionsへ記録する。
5. focused testを先に実行し、その後に関連crate/workspaceの検証を行う。
6. 変更したプロダクションコードを`rust-coding` Policyへ照合する。
7. command quality gateの失敗を根拠に修正し、未通過のまま完了を宣言しない。

レビューID、修正round、内部推論をソースコメントへ残さないでください。
