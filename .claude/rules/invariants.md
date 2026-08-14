# 不変条件

CLAUDE.md に列挙した前提の、根拠と検証方法。**変更が必要だと思ったら、まず root に相談してください。**
これらは「今のところそうなっている」実装詳細ではなく、破ると別の場所が静かに壊れる契約です。

## 1. 依存は内向きのみ

```
cli → application → domain
cli → infrastructure → domain
```

`application` の `Cargo.toml` に `agent-infrastructure` は**現れません**。
ユースケースが HTTP やファイルシステムに直接触ることが構造的に不可能になります。

**tokio も同様に禁止**です。タイマーやランタイムが必要な関心事（タイムアウト、リトライ）は
`infrastructure` の装飾子として実装します（`TimeoutTool`, `RetryingProvider`）。
この制約があるおかげで `crates/application/tests/agent_loop.rs` はランタイム抜きでループ全体を検証できます。

**新しい能力が必要になったら** — `domain/src/ports/` に trait を足し、`infrastructure` で実装し、
`cli/src/composition.rs` で結線します。ショートカットはありません。

検証:
```bash
make exec CMD="grep -rn 'tokio\|agent_infrastructure' crates/application/src/ | grep -v '^.*://'"
```

## 2. ファイルアクセスは `WorkspacePath` 経由のみ

`WorkspacePath` を持たない限りファイルには触れません（`FileSystem` ポートが要求する）。
生成できるのは `WorkspaceRoot::resolve()` だけです。

防御は 2 段構えで、**両方必要**です。

| 段 | 場所 | 防ぐもの |
|---|---|---|
| 字句 | `domain/src/model/workspace.rs` | `..` による脱出、範囲外の絶対パス、NUL |
| 正規化 | `infrastructure/src/fs/local.rs` の `guard()` | ワークスペース内から外部を指すシンボリックリンク |

`guard()` には既知の落とし穴があります: 既存パスに空の tail を `join` すると末尾スラッシュが付き、
通常ファイルへの全 syscall が ENOTDIR になります。早期 return で回避済みなので消さないでください。

**この不変条件は子プロセスには効きません。** `run_command` が起動したプロセスは
`WorkspacePath` の外側にいるため、封じ込めは OS が行います（`infrastructure/src/exec/`）。
ここを触るときの契約:

- **黙って弱くならない** — 要求された封じ込めが得られない場合は
  `CommandError::SandboxUnavailable` で起動を失敗させる。フォールバックを足さないでください
- **`CommandRunner::sandbox()` は実際に効いているものを返す** — 設定値を返さない。
  `agent doctor` とツール説明はこちらを表示します
- **外部バイナリに依存しない** — CLI/SDK として配布するため、`apt install` が要る機構は
  実質的に無効なサンドボックスと同じです
- **サンドボックスの主張はテストで実証する** — ルールが「構築できる」ことではなく、
  実際の子プロセスが**できなかった**ことを `crates/infrastructure/tests/sandbox.rs` で固定する。
  制限を回避できない場合は、制限そのものを assert して残す

## 3. ツール失敗は実行失敗ではない

未知のツール、引数不正、承認拒否、タイムアウト — すべて `is_error: true` の `tool_result` として
モデルに返り、ループは継続します。モデルが自分の誤りから復帰するのは例外ではなく通常動作です。

`AgentLoop::run` が `Err` を返すのは**プロバイダ自体が壊れている場合のみ**です。

エラーメッセージは人間ではなく**モデル向け**に書きます。何が失敗したかより、次に何をすべきかを書いてください。

```rust
// 悪い例
"file not found"
// 良い例
"`old_string` was not found in `{path}`. Read the file again and copy the excerpt exactly - whitespace and indentation must match."
```

## 4. すべての `tool_call` に結果を返す

対応する `tool_result` を欠いた履歴は、次のリクエストでプロバイダに拒否されます。
`ToolDispatcher::dispatch` は入力の呼び出し数と同じ長さの `Vec<ToolResult>` を、**要求順で**返します。

同じ理由で `Conversation::trim_to_budget` は、刈り込み後の先頭に孤児の `tool_result` を残しません。

並列化してよいのは読み取り系ツールだけです。同一ターンの 2 つの書き込みは同じファイルを触りうるため、
要求順に直列実行します。

## 5. テストはネットワークも実モデルも要求しない

| 層 | 手法 |
|---|---|
| domain | 純粋な単体テスト |
| application | 全ポートをフェイク（`tests/agent_loop.rs` の `ScriptedProvider` など） |
| infrastructure（設定） | `MapEnv` を注入。プロセス環境は触らない |
| infrastructure（HTTP） | `agent-test-support` の生 TCP モックサーバ |
| E2E | 上記モック + 実ファイルシステム（`crates/cli/tests/end_to_end.rs`） |

`std::env::set_var` を使うテストを追加しないでください。プロセス環境はグローバルな可変状態で、
並列実行するテスト同士が干渉します。設定は `Settings::from_source(&MapEnv::new(...))` で注入します。

## 6. `cargo` をホストで実行しない

`build.rs` と proc-macro は crates.io から取得したコードを**その場で実行**します。
ホストの認証情報（`~/.ssh`, `~/.aws`, keychain）に届かせないため、すべて `make` 経由でコンテナ内に閉じ込めます。
`.claude/settings.json` がホストでの `cargo` 直接実行を拒否します。

依存を足すときも `make cargo CMD="add --package agent-cli <crate>"` です。
新しい依存は、それ自体がサプライチェーンの表面積です — 60 行で書けるものに依存を足さない方針です
（`crates/test-support` のモックサーバが生 TCP なのはこのため）。
