# Rust Audit Standard

## 性質

このKnowledgeは、Rust成果物を評価するための監査規格です。
以下の記述は監査Agentへの実装命令ではなく、監査対象となる成果物、変更、テスト、品質証拠について判定する命題です。
Criteria IDは監査間で安定させ、計画時に再採番または意味変更しません。

## Source Manifest

- Standard Revision: `1`
- Compiled At: `2026-07-16`
- `rust-design.md`: `b688224f0765c85446a4abef403bacdd22b7a4bcfc97e8994591d69dd0141947`
- `rust-coding.md`: `f5269afac4d704003ab244c9d0e38e0f4ff44e746a6405a20d0c0240186e4c31`
- `rust-testing.md`: `bc2323b9f3b2b8602f60ec3c9a3f2108fcb2f5e09ef0af8dee22db1ef3cc3693`
- `rust-review.md`: `570a4e61aa2addeaf67425d453e259a738a1d68b6db6c83815c6055f2ef7714f`

元Policyの内容が変わった場合、このKnowledgeは鮮度不明として扱い、規格を再コンパイルするまで新旧どちらかを勝手に正としません。

## 判定語彙

- `compliant`: 必要な証拠があり、期待特性を満たす
- `drift`: 違反条件を裏付ける具体的証拠がある
- `unknown`: 適用可能だが証拠不足
- `not_applicable`: 対象成果物または変更へ適用されない

## Criteria

### POL-ARC-001 — 既存境界と契約の整合
- Source: `rust-design:3-4`
- 規範主体: workspace、crate、feature、公開API、error型、async runtime、テスト構成
- 期待特性: 主要な責務境界と公開契約が相互に整合し、実装が既存構造を理由なく迂回していない。
- 適用対象: design / architecture / workspace / API
- 判定種別: semantic
- 必要な証拠: Cargo構成、主要module、公開API、仕様・ADR、呼び出し関係
- 違反条件: 同一責務の重複、境界の逆転、既存契約と説明不能な構造差分が確認できる。

### POL-ARC-002 — 不要な拡張の抑制
- Source: `rust-design:5`, `rust-coding:6-9`
- 規範主体: crate、dependency、公開API、一般化、リファクタリング
- 期待特性: 明示要求または既存契約に必要な範囲を超える拡張が混在していない。
- 適用対象: design / dependencies / API / task drift
- 判定種別: semantic
- 必要な証拠: タスク目的、差分・履歴、Cargo.toml、公開API変更、設計文書
- 違反条件: 将来用途だけの追加、無関係な全面整理、説明不能な依存・公開面の増加がある。

### POL-ARC-003 — 抽象化の責務と不変条件
- Source: `rust-design:6-9`, `rust-coding:35-43`, `rust-review:16-17,24`
- 規範主体: type、trait、NewType、module、constructor、型中心操作
- 期待特性: 新規または主要な抽象化が具体的責務、不変条件、公開境界、既存patternのいずれかを持ち、型中心操作は原則inherent implへ置かれる。
- 適用対象: design / types / traits / modules / public API
- 判定種別: semantic
- 必要な証拠: 型定義、impl、利用箇所、公開操作、不変条件、既存pattern
- 違反条件: 名前だけのwrapper、単一用途trait、責務のないmodule、誤用防止能力のない抽象化がある。

### POL-SCP-001 — 監査対象変更と既存違反の分離
- Source: `rust-design:12`, `rust-coding:6-9`
- 規範主体: task scope、変更履歴、quality gate blocker
- 期待特性: 現在の目的に必要な変更と無関係な既存問題が分離され、既存違反がscope拡大の理由として混入していない。
- 適用対象: task drift / history / planning evidence
- 判定種別: semantic
- 必要な証拠: タスクキュー、git履歴、現在差分、Finding依存関係
- 違反条件: 一つの変更・候補へ独立した既存問題が混在し、個別検証できない。

### POL-EVD-001 — 設計判断の追跡可能性
- Source: `rust-design:13`, `rust-review:9`
- 規範主体: 非自明な設計判断とPolicy例外
- 期待特性: 設計判断または例外が既存実装の`file:line`、明示要件、外部契約、テスト、文書のいずれかに追跡できる。
- 適用対象: design / exceptions / decisions
- 判定種別: semantic
- 必要な証拠: ADR、仕様、近傍コメント、テスト、外部契約参照
- 違反条件: 重要な例外・逸脱の成立根拠がなく、妥当性を再検証できない。

### POL-ERR-001 — Production panic shortcutの制御
- Source: `rust-design:10-11`, `rust-coding:13-14`
- 規範主体: production Rust codeの失敗可能処理
- 期待特性: 新規`.unwrap()`がなく、回復可能な失敗は`Result`と`?`等で伝播され、`expect()`は不変条件と成立理由に対応する。
- 適用対象: implementation / error handling
- 判定種別: both
- 必要な証拠: clippy結果、該当式、周辺不変条件、呼び出し元
- 違反条件: production codeのunwrap、理由のないexpect、回復可能な失敗のpanic化がある。

### POL-ERR-002 — Error契約の型情報と原因連鎖
- Source: `rust-coding:15-17`, `rust-review:13`
- 規範主体: production API、domain error、error conversion
- 期待特性: 公開・伝播error APIがdomain上の失敗を識別でき、型情報と原因連鎖を不要に失わない。
- 適用対象: API / error types / conversion boundaries
- 判定種別: semantic
- 必要な証拠: function signature、error enum、From変換、source chain、利用側match
- 違反条件: `Box<dyn Error>`相当の不要な消去、文字列化による識別不能、既存domain errorの迂回がある。

### POL-UNS-001 — Unsafeの限定と安全性契約
- Source: `rust-design:10-11`, `rust-coding:21-23`, `rust-review:14`
- 規範主体: unsafe block、unsafe fn、unsafe trait、unsafe impl
- 期待特性: unsafeはsafe Rustで要件を満たせない箇所へ限定され、実操作に対応する不変条件と利用者・実装者の義務が文書化される。
- 適用対象: implementation / unsafe API
- 判定種別: both
- 必要な証拠: clippy結果、`SAFETY:`近傍、API docs、呼び出し条件
- 違反条件: 根拠のないunsafe、操作と不一致な説明、公開unsafe契約の欠落がある。

### POL-MOD-001 — Moduleと公開型の凝集性
- Source: `rust-coding:27-31`, `rust-review:15`
- 規範主体: module file、`mod.rs`、同一fileの`pub struct`
- 期待特性: file/module境界が責務と変更理由を反映し、新規`mod.rs`を導入せず、複数公開型の同居には密接な関係がある。
- 適用対象: modules / file layout / public types
- 判定種別: both
- 必要な証拠: 現在差分・git履歴、file内の公開型数、利用・変更関係
- 違反条件: 新規mod.rs、理由のない公開型集中、Policy回避だけの細分化が確認できる。履歴不明の既存mod.rsだけではdriftとしない。

### POL-API-001 — 公開境界のdomain表現
- Source: `rust-coding:35-40`, `rust-review:16,24`
- 規範主体: public APIと同期・配送・保存等の実装詳細
- 期待特性: 公開境界はdomain上の意味、許可操作、不変条件を表し、必要な場合に限り実装詳細をNewTypeで隠蔽する。
- 適用対象: public API / concurrency / messaging / storage
- 判定種別: semantic
- 必要な証拠: public signature、wrapper API、field visibility、利用側操作
- 違反条件: `Arc<Mutex<_>>`等の用途固定実装詳細が契約として漏れる、または名前だけのwrapperが増える。

### POL-API-002 — Method名とownership semantics
- Source: `rust-coding:41-48`, `rust-review:17-18`
- 規範主体: constructor、conversion、method naming
- 期待特性: 型中心操作は適切なimplに属し、`as_`/`to_`/`into_`/`_mut`/`is_`が実際のownershipと副作用に一致する。
- 適用対象: API / methods / ownership
- 判定種別: both
- 必要な証拠: method signature、receiver型、戻り値、clippy結果、利用例
- 違反条件: 名前とreceiver/生成/消費 semanticsが不一致、型中心の公開free functionが不必要に分散する。

### POL-ASY-001 — Async表現の直接性
- Source: `rust-design:10-11`, `rust-coding:49-51`, `rust-review:22`
- 規範主体: async APIとFuture返却関数
- 期待特性: async blockを返すだけの関数は原則`async fn`で表現され、manual Futureの例外は外部契約等の具体的理由を持つ。
- 適用対象: async implementation / public API
- 判定種別: both
- 必要な証拠: clippy結果、signature、object safety・Send・trait契約の根拠
- 違反条件: 理由のないmanual Future、例外理由と実際の契約の不一致がある。

### POL-GEN-001 — Genericとboundの必要性
- Source: `rust-coding:52-53`, `rust-review:19`
- 規範主体: generic parameter、trait bound、where clause
- 期待特性: genericは現在の具体的用途を持ち、bound配置が単一はinline、複数はwhere句という規約と可読性に整合する。
- 適用対象: generics / API / implementation
- 判定種別: semantic
- 必要な証拠: generic利用箇所、monomorphized用途、signature、既存契約
- 違反条件: 将来用途だけのgeneric、不要な抽象化、説明不能なbound配置がある。

### POL-CMT-001 — コメントの情報価値
- Source: `rust-coding:57-58`, `rust-review:20-21`
- 規範主体: source commentと近傍documentation
- 期待特性: コメントは理由、不変条件、外部契約、非自明な制約を説明し、codeの言い換え、banner、AI tracking markerを含まない。
- 適用対象: source comments / documentation
- 判定種別: both
- 必要な証拠: 構造検査結果、該当コメントと周辺code
- 違反条件: AI marker、装飾区切り、変更round記録、情報を追加しない逐語説明がある。

### POL-TST-001 — 契約中心のテストcoverage
- Source: `rust-testing:4,11,13`
- 規範主体: unit/integration testとassertion
- 期待特性: 観測可能な振る舞い、状態遷移、error契約、不変条件、重要な異常系が意味まで検証される。
- 適用対象: tests / public contracts / failure modes
- 判定種別: semantic
- 必要な証拠: 仕様・受け入れ条件とtest caseの対応、assertion、error variant検証
- 違反条件: 重要契約が未検証、`is_err()`だけで意味を検証しない、実装手順だけを固定する。

### POL-TST-002 — Test seamの非侵食
- Source: `rust-testing:5-6`
- 規範主体: test helper、fixture、production API、feature/runtime構成
- 期待特性: テストは既存harnessを再利用し、テスト都合だけのproduction APIや抽象化を増やさない。
- 適用対象: tests / API / fixtures / features
- 判定種別: semantic
- 必要な証拠: helper・fixture、cfg/feature、公開API、test-only seam
- 違反条件: テスト専用都合で公開契約が拡大、既存harnessと重複する基盤が増加する。

### POL-TST-003 — Test内の失敗処理
- Source: `rust-testing:7-8`
- 規範主体: test codeのsetup、helper、前提確認
- 期待特性: `.unwrap()`を追加せず、前提失敗は具体的な`expect()`、伝播可能な失敗は`Result`で表現される。
- 適用対象: tests / test helpers
- 判定種別: both
- 必要な証拠: clippy対象範囲、test code、failure message
- 違反条件: test unwrap、意味のないexpect、原因を失うpanicがある。

### POL-TST-004 — Testの決定性
- Source: `rust-testing:9-10`
- 規範主体: time、random、ordering、network、filesystem、async synchronization
- 期待特性: 外部状態や実行タイミングへの依存が制御され、sleep延長で安定化していない。
- 適用対象: tests / async tests / integration tests
- 判定種別: semantic
- 必要な証拠: test setup、clock/RNG注入、tempdir、network mock、同期条件
- 違反条件: 暗黙外部依存、順序依存、固定sleepによる競合回避、再現不能なflakinessがある。

### POL-TST-005 — Redとassertionの診断性
- Source: `rust-testing:11-12`, `rust-coding:59`
- 規範主体: test name、assertion、fixture/import、期待値変更
- 期待特性: 失敗時に破られた契約が分かり、Redは未実装要件を直接示し、テスト通過目的で検証を弱めない。
- 適用対象: tests / TDD evidence / assertions
- 判定種別: semantic
- 必要な証拠: test名、失敗message、assertion差分、受け入れ条件、履歴
- 違反条件: fixture/import不備によるRed、曖昧なassertion、要件を変えず期待値だけ緩和した証拠がある。

### POL-QA-001 — Quality gate実行とcoverageの分離
- Source: `rust-coding:8-9,60`, `rust-review:5-6`
- 規範主体: canonical quality gate、CI、完了証拠、semantic review
- 期待特性: repository全体の決定的検査は正規gateで記録され、gate結果とPolicy coverageの十分性が別々に評価される。
- 適用対象: quality gate / CI / completion evidence
- 判定種別: both
- 必要な証拠: `rust-quality.sh`、mechanical-check.md、raw log、CI、Criteria-to-check対応
- 違反条件: gate未実行の完了宣言、PASSだけを全Criteria準拠の証明にする、FAILを原因分類せず無視する。

## Agent向け文言として除外したSource Clause

次は元Policyの役割制御であり、製品成果物のCriteriaへは変換していません。

- `rust-review:3-8`のactive instruction優先、独自出力形式禁止、Finding記述形式
- `rust-testing:3`のテストstep中にproduction codeを変更しないという実行権限制約
- 各Policyの「提案する」「完了を宣言する」等のAgent行動は、Audit Control Policy側で必要な範囲だけ制御する
