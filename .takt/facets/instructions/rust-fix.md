{extends:fix}

## Rust固有の修正手順

### Architecture Review
{report:architect-review.md}

### AI Antipattern Review
{report:ai-antipattern-review.md}

### Rust Coding Review
{report:coding-review.md}

1. 各指摘について、根拠、現在の証拠、必要な変更、検証方法を対応付ける。
2. 現在のコードで確認できる問題だけを修正する。
3. 複数指摘が同じ原因なら、原因を一度直して全指摘を再検証する。
4. 公開API、error型、module境界を変更する場合は、呼び出し元とテストへの影響を追う。
5. レビュアーを満たすためだけの抽象化、単一用途trait、名前だけのNewType、
   説明目的だけのコメントを追加しない。
6. focused test、関連crate test、command quality gateの順に検証する。
7. レビューID、round、内部推論をソースへ残さない。
