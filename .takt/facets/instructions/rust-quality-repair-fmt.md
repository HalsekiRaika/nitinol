現在失敗しているformatting検査だけを修復してください。

1. タスクと機械検査ログから`cargo fmt --all -- --check`の失敗を確認する。
2. `cargo fmt --all`を実行する。
3. formatterによる差分以外の意味的変更を加えない。
4. 差分を確認し、対象外ファイルへ意図しない変更がないことを確認する。
5. Workflowの限定quality gateが成功する状態にする。

full `rust-quality.sh`の後続検査が失敗しても、このタスクへ混ぜないでください。
