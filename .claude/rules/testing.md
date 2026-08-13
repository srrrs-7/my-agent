# テスト方針

## 層ごとの手法

| 層 | 手法 | 例 |
|---|---|---|
| domain | 純粋な単体テスト | パス脱出の拒否、履歴刈り込みの不変条件、文字境界 |
| application | 全ポートをフェイク | ループの分岐、並列実行の順序保証、承認拒否 |
| infrastructure（設定） | `MapEnv` を注入 | 別名の大文字化、空文字の扱い、キーのフォールバック |
| infrastructure（写像） | 単体テスト | `finish_reason:"stop"` + `tool_calls` の解釈 |
| infrastructure（HTTP） | `agent-test-support` のモックサーバ | ステータス写像、実際に送出される JSON |
| 全体 | E2E | HTTP → ループ → 実ファイル書き込み |

フェイクは `crates/application/tests/agent_loop.rs` に揃っています
（`ScriptedProvider`, `MemoryFileSystem`, `AlwaysApprove` / `AlwaysDeny`, `RecordingSink`）。
新しいフェイクを書く前に、そこにあるものを使えないか確認してください。

HTTP レベルのモックは `crates/test-support` です。infrastructure と cli の両方から使えます。

## 何をテストするか

**振る舞いの契約をテストし、実装をテストしない。** 具体的には次を優先します。

- 不変条件（`invariants.md` の各項目に対応するテストがあるか）
- 実装が食い違う境界（`arguments` が文字列でもオブジェクトでも通る、など実際のサーバの差異）
- エラー経路（モデルに返るメッセージが、次に何をすべきか言えているか）
- 退行しやすい細部（文字境界、末尾スラッシュ、順序保証）

テスト名は「何が保証されるか」を文で書きます。

```rust
// 良い
fn never_leaves_an_orphaned_tool_result_at_the_head()
fn a_denied_call_tells_the_model_why()
// 悪い
fn test_trim()
```

`assert!` には失敗時に原因がわかるメッセージを付けます: `assert!(cond, "got {value:?}")`。

## 禁止事項

- `std::env::set_var` を使う設定テスト（`MapEnv` を注入する）
- ネットワークに出るテスト、実モデルを要求するテスト
- `sleep` で待つテスト（タイミング依存は `tokio::time` かモックで解決する）

## 実行

```bash
make test                                            # 全部
make cargo CMD="test -p agent-domain"                # crate 単位
make cargo CMD="test -p agent-cli --test end_to_end" # 統合テスト単位
make cargo CMD="test --workspace <部分一致する名前>" # 単体
make cargo CMD="test --workspace -- --nocapture"     # 出力を見る
```

失敗を調査するときは `RUST_BACKTRACE=1 make test` が使えます
（既定は 0 にしてあります。CLI のエラー表示を読みやすくするため）。
