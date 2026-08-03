現在失敗しているClippy検査だけを修復してください。

- 機械検査ログから最初の再現可能なClippy failureと原因箇所を特定する。
- `cargo clippy --workspace --all-targets`と現在のdeny規則を満たす最小変更を行う。
- 安易な`#[allow(...)]`、warning抑制、対象除外で回避しない。
- 公開契約や意味的挙動を変える場合は、タスク根拠との対応を明示して関連テストを確認する。
- Workflowの限定quality gateが成功する状態にする。

fmt、dylint、test、structuralの既存failureは同時に修正しないでください。
