現在失敗しているstructural checkだけを修復してください。

対象はcanonical `rust-quality.sh`のdeterministic structural checksです。

- `src/**/mod.rs`、AI review-marker comment、装飾separator commentのうち、ログで確認された違反だけを扱う。
- module移動では公開module path、`use`、tests、examples、docsへの影響を確認する。
- コメント削除では、必要な仕様情報まで失わないよう、適切な文書または通常コメントへ整理する。
- structural検査を緩めたり除外を追加したりせず、Workflowの限定quality gateを成功させる。

他のquality check failureはこのタスクへ混ぜないでください。
