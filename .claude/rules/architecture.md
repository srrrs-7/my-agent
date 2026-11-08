# アーキテクチャ規約

「どこに何を書くか」の判断基準。設計判断の**背景**は `management/docs/architecture.md` にあります
（そちらはリポジトリの読者向け、こちらは変更を加える側向け）。

## 層とその責務

| crate | 責務 | 許可される依存 |
|---|---|---|
| `agent-domain` | エンティティ、値オブジェクト、ポート(trait)、エラー語彙、共有カーネル(`text`) | std, serde, serde_json, thiserror, async-trait, futures-core(Stream trait のみ) |
| `agent-application` | ユースケース: ループ、ディスパッチャ、プロンプト組み立て、ツール実装 | domain, async-trait, futures, serde, tracing |
| `agent-infrastructure` | アダプタ: LLM の HTTP クライアント、FS、検索、設定、テレメトリ | domain, reqwest, tokio, futures, bytes, ignore, regex, chrono |
| `agent-sdk` | 合成ルート（`unstable::assemble`。DI はここだけ）と埋め込み用の安定 API（`Agent` / `Session` / `Interrupt` / `Outcome` / `events` DTO） | domain, application, infrastructure, anyhow, async-trait, futures, serde, serde_json, tracing, uuid |
| `agent-cli` | 引数解析、描画、承認 UI（`CliApprovalGate`）、通知、Ctrl-C、REPL、`.env`。sdk の上の薄い層 | 全て |
| `agent-test-support` | テストダブル | tokio, serde_json |

`agent-application` に `tokio` を足さないでください（`invariants.md` §1）。

## 置き場所の判断

| 追加するもの | 置き場所 |
|---|---|
| 新しい概念・不変条件を持つ値 | `domain/src/model/` |
| 外界との新しい境界 | `domain/src/ports/` + `infrastructure` に実装 + `sdk/src/unstable.rs`（合成ルート）に結線 |
| 端末に固有の注入（承認の問い方・描画・通知） | `cli/src/composition.rs`。sdk の `Wiring::asks` / `events` 経由で渡す |
| 埋め込み向けの公開 API | `sdk/src/`（`unstable` 以外）。公開 enum は `#[non_exhaustive]`、ドメイン型をそのまま安定 API にしない |
| モデルに公開する新しい能力 | `application/src/tools/` （`/add-tool` スキル参照） |
| 新しい LLM ベンダ | `infrastructure/src/llm/` （`/add-provider` スキル参照） |
| 横断的関心事（タイムアウト・リトライ・計測） | `infrastructure` の装飾子。ポートを実装して内側をラップする |
| 表示・整形 | `cli/src/render.rs` または `cli/src/commands/` |
| 複数層で使う純粋関数 | `domain/src/text.rs`（共有カーネル）。増やす前に本当に共有かを疑う |

## ポートと実装の対応

| ポート | 実装 |
|---|---|
| `LlmProvider` | `OpenAiCompatibleProvider`, `AnthropicProvider`, `RoutingProvider`, `RetryingProvider`, `FailoverProvider` |
| `LlmRouter` | `StaticRouter`, `ModelPrefixRouter`, `TaskKindRouter` |
| `Tool` | `application/src/tools/{file,web,exec,plan}/*`（実装）, `TimeoutTool`・`RedactingTool`・`DependencyWatchTool`（装飾） |
| `FileSystem` | `LocalFileSystem` |
| `FileSearcher` | `IgnoreAwareSearcher` |
| `ContextProvider` | `WorkspaceContextProvider` |
| `PromptBuilder` | `DefaultPromptBuilder`, `FixedPromptBuilder`（差し替え）, `AppendingPromptBuilder`（追記・装飾）, `SubagentPromptBuilder`（サブエージェントの定型） |
| `ApprovalGate` | `CliApprovalGate`（`authorize_dependency_change` は既定で拒否）。装飾子 `PlanModeGate` / `RuleGate`（application）・`HookGate`（infrastructure、前フック）。積み順 PlanMode → Hook → Rule → Cli、依存承認は必ず内側へ転送。拒否理由は「誰が」拒否したかを含める（ディスパッチャの文面は中立） |
| `BackgroundCommands` | `BackgroundSupervisor`（`run_command` と同じ封じ込め・寿命と出力の上限・drop で全ジョブ停止） |
| `WebFetcher` | `GuardedWebFetcher`（SSRF ガード付き・既定無効） |
| `CommandRunner` | `SandboxedCommandRunner`（Linux: Landlock + egress プロキシ・既定無効） |
| `ToolServer` | `McpServer`（MCP over stdio。サーバは Landlock の中・既定無効。命名・安全区分・無害化は application の `tools/server.rs`） |
| `EventSink` | `TerminalRenderer`, `NullEventSink`, `MetricsSink`（eval の計測） |
| `ConversationLog` | `FileConversationLog`, `NullConversationLog`（会話の記録。追記のみ） |
| `SessionStore` | `FileSessionStore`（同じ記録の読み出し・一覧・削除） |
| `Notifier` | `WebhookNotifier`（ntfy / Slack / json・既定無効）, `NullNotifier`。発火は cli のみ（承認ゲートと run / chat） |
| `RemoteApprover` | `NtfyRemoteApprover`（返信トピックを外向きに購読・起動時に匿名アクセス不可を実証）, `NullRemoteApprover`。競合は `CliApprovalGate` の中 |
| `WebSearcher` | `BraveSearcher`（既定無効・Network 区分で毎回承認） |

`LlmProvider` が合成パターンになっている点が重要です。ルータもフェイルオーバーもリトライもそれ自体が
`LlmProvider` なので、呼び出し側は積み方の変化を認識しません。積み順は外側から
Routing → Failover → Retry → クライアントです（`infrastructure/src/llm/mod.rs`, `factory.rs`）。

```
Arc<dyn LlmProvider>
  = RoutingProvider({                                   // 宛先をリクエストごとに選ぶ
      "cloud": FailoverProvider(                        // 宛先をまたぐ（opt-in）
                 RetryingProvider(Anthropic…),          // 同じ宛先でのバックオフ
                 [RetryingProvider(OpenAi…)]),
      "local": RetryingProvider(OpenAi…) })
```

リトライは宛先ごとの内側、フェイルオーバーはその外側です。各宛先が自分のリトライを使い切ってから
初めて代替先に移ります。`AGENT_FAILOVER` を設定しなければ宛先は `RetryingProvider(client)` だけです。

将来のルーティング（コスト・レイテンシ・能力ベース、A/B）は `LlmRouter` の実装追加だけで足ります。
フェイルオーバーはルータではなく `FailoverProvider` 装飾子の側で、宛先の中で代替先を持ちます。
判断材料の `RequestMetadata`
（`task_kind` / `iteration` / `requires_tools` / 任意の `hints`）は全リクエストに同行しています。
**プロンプト文字列を解析して分岐するルータを書かないでください** — その材料は既にあります。

## 履歴の圧縮

`fit_history` は 2 段です。圧縮（`agent/compaction.rs`）が意味を保つ方、刈り込み
（`Conversation::trim_to_budget`）が予算を保証する方で、後者は常に走ります。
計画メモの分の予算の確保と、短くした後にメモを再掲するかの判断は `agent/plan.rs`
（`plan::reserve` / `Reservation::restore`）にあり、`fit_history` は圧縮 → 刈り込み → 追記だけを行います。

境界の計算は domain（`compaction_split` / `replace_prefix`）にあります。
「どこで切ってよいか」は `tool_call` / `tool_result` の対を壊さないという不変条件
（`invariants.md` §4）そのものだからです。**要約の中身を決めるコードを domain に置かないでください** —
それはユースケースです。

## `Tool::preview` と依存の承認

`Tool::preview` は「この引数で書き込むファイルと、書き込み後の内容」を返します。**既定実装はありません。**

- ファイルを書くツールは、`execute` と同じ計算（共通の関数）で予定を返す。承認された内容と書かれる内容がずれないように
- 書かないツール（読み取り・コマンド・ネットワーク）は空の `Vec` を返す
- **装飾子は必ず内側に転送する**。空を返すと、その装飾子越しの書き込みが依存の承認を素通りする
  （`sdk/src/unstable.rs` のテストが Redacting(Timeout(write_file)) で固定）

ディスパッチャは Phase 2 の各 invoke 直前に `preview` を呼び、依存ファイル（`ManifestKind::classify`）の
中身が変わるなら `ApprovalGate::authorize_dependency_change` を呼びます。差分の説明は
`application/src/supply_chain/` です。**説明の成否で承認を飛ばす分岐を書かないでください。**

コマンド経由の変更は `DependencyWatchTool`（infrastructure）が事後に検知します。包む対象は
**安全区分が `Destructive` のツールすべて**で、名前では選びません。`TimeoutTool` の外側・`RedactingTool` の内側に置きます。

## ループとディスパッチャの分割

`loop_runner.rs` はループ制御だけ、`dispatch.rs` はツール実行だけを持ちます。
ディスパッチャは独自の不変条件（`invariants.md` §4）とテストを持つ独立した関心事です。

ループに手を入れるときは、追加しようとしている処理が
「モデルとの往復の制御」なのか「1 回のツール実行の扱い」なのかを先に決めてください。

## 出力チャネル

ループは**何も出力しません**。`AgentEvent` を発行し、描画方法は `EventSink` の実装が決めます。

- モデルの散文 → **stdout**
- ツール活動・警告・承認プロンプト → **stderr**

`agent run "..." > answer.md` が期待どおり動くのはこの分離のためです。壊さないでください。

**`agent run --output-format json|stream-json` では stdout は JSON だけ**です（`cli/src/output/`）。
散文もストリーミングの断片も stdout に出しません — 回答は結果オブジェクトの `final_text` に入ります。
stderr はそのまま（`TerminalRenderer` から散文のイベントだけを除いて転送）。
JSON の形は SDK の DTO（`sdk/src/events.rs`、cli からは `output::dto` として再公開）が決め、
**`AgentEvent` やドメイン型に `Serialize` を付けません**。この DTO は SDK の安定スキーマでもあります。
ドメインの改名が公開スキーマを黙って変えないためです。全行に `"v":1` が付きます。
起動失敗・プロバイダの失敗・中断を含め、どの終わり方でも最後に `"type":"result"` の 1 行を出します。

JSON 内の文字列（モデルの散文・ツール引数・要約）は**値を変えず**に渡します。`printable` は掛けず、
代わりに `is_unprintable` な文字を `\uXXXX` でエスケープして書きます（パース後の値は同一、
生のバイトは端末を書き換えない）。

stderr に出す文字列のうち、モデルやサーバ（MCP サーバ・LLM プロバイダ・ファイルの中身）が決めたもの
— ツール引数、結果の要約、計画の項目、ファイル名、拒否理由など — は、**出口で一度だけ**
`domain::text::printable` を通します。制御文字・双方向制御・不可視文字が端末の表示を書き換え、
人間が見たものと承認したものがずれるのを防ぐためです。stdout のモデルの散文には掛けません
（それは回答そのもので、複数行でありえ、それを根拠に何かを承認するわけではないため）。

## 設定

`Settings::from_source(&dyn EnvSource)` が本体、`from_env()` は薄いラッパです。
新しい設定項目を足すときは 2 箇所を同時に更新します。

1. `infrastructure/src/config/` — 読み取りと既定値
2. `README.md` §8 — 変数表。**全変数の説明・注意書きの唯一の置き場**です

**能力を広げる・封じ込めを緩める設定**（ツールを有効にする、サンドボックスや承認を緩める、外部へ送る、
ワークスペースやプロンプトの読み先を変える）を足したら、`cli/src/dotenv.rs` の `DENIED` にも加えます。
`.env` はリポジトリに同梱されうるため、そうした値は実環境変数からしか受け取りません。

`.env.example` は必須項目（`AGENT_PROVIDER` / `AGENT_API_KEY` / `AGENT_MODEL`）だけを載せる
最小の雛形です。**必須項目が変わるときだけ**更新します。任意の設定を足しても触りません。

必須項目を増やさないでください。新しい設定には既定値を持たせます。接続先の既定値は
プロバイダ名から決まります（`config/kinds.rs` の `ProviderPreset`。`ProviderKind` は wire
プロトコルで、`ollama` と `openai` を区別しません）。

`make exec CMD="grep -rho 'AGENT_[A-Z_]*' crates/ | sort -u"` で実装との差分を確認できます。
