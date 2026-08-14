# コード規約

`rustfmt.toml`（edition 2024 / max_width 100）と clippy `-D warnings` が形式面を担保します。
ここに書くのは自動化できない部分だけです。

## コメント

コメントは**なぜそうなっているか**を書きます。何をしているかはコードが言えるはずです。

```rust
// 悪い: コードの言い換え
// tail が空なら canonical を返す
if tail.as_os_str().is_empty() { return Ok(canonical); }

// 良い: 消したら壊れる理由
// Joining an empty path appends a trailing separator, and a trailing slash on
// a regular file makes every syscall fail with ENOTDIR.
if tail.as_os_str().is_empty() { return Ok(canonical); }
```

モジュールレベルの `//!` には、そのモジュールが存在する理由と、
外から見えない前提（例: 「Ollama は tool_calls があっても finish_reason に stop を返す」）を書きます。

**コメントは英語**です。ユーザー向けドキュメント（`README.md`, `docs/`, `.claude/`）は日本語です。

## エラーメッセージ

読み手が誰かで書き分けます。

| 誰が読むか | 例 | 方針 |
|---|---|---|
| LLM（`ToolError`） | `edit_file` の不一致 | 次に何をすべきかを書く |
| 人間（`ConfigError`） | `AGENT_MODEL` 未設定 | 変数名と、取りうる値の例を書く |
| 開発者（`LlmError`） | HTTP 502 | 生の情報を保持しつつ 500 バイトで切る |

## 型

- 検証を持つ文字列は newtype にする（`ToolName`, `ModelId`, `ProviderId`, `WorkspacePath`）
- `serde` は `#[serde(try_from = "String", into = "String")]` で newtype の検証を通す
- ポートの trait は `dyn` 互換に保つ（`async_trait` を使う）
- 外部 API の応答は `#[serde(default)]` を厚めに付ける。実装ごとの差異でパース全体を落とさない
- 未知の列挙値は落とさず無視する（Anthropic の `#[serde(other)] Unknown` を参照）

## 命名

- ポートの実装は「何であるか」で命名する（`LocalFileSystem`, `IgnoreAwareSearcher`）
- 装飾子は `-ing` / 役割名（`RetryingProvider`, `TimeoutTool`, `RoutingProvider`）
- `&self` を取るメソッドに `into_*` を使わない（clippy が落とす）。`decode` などにする

## 避けること

- `unwrap()` / `expect()` をライブラリコードに置く。テストと、静的に自明な場合（`ToolName::new("read_file")`）は可
- `std::process::exit`（デストラクタとフラッシュを飛ばす）。`ExitCode` を返す
- 同じ小さなヘルパを層ごとに再実装する。共有カーネル `domain/src/text.rs` を先に見る
- 何もしない防御的コード。到達不能なら理由をコメントに書き、そのうえで安全側に倒す
