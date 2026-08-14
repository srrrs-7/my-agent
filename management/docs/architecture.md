# アーキテクチャ

## 1. レイヤ構成

```
┌──────────────────────────────────────────────────────────────┐
│ agent-cli                    プレゼンテーション + 合成ルート  │
│   args.rs        clap 定義                                    │
│   render.rs      AgentEvent → 端末出力                        │
│   approval.rs    対話的な承認ゲート                           │
│   composition.rs 具象型とポートを結線する唯一の場所           │
└───────────────┬──────────────────────────┬───────────────────┘
                │                          │
┌───────────────▼──────────────┐ ┌─────────▼───────────────────┐
│ agent-application            │ │ agent-infrastructure        │
│  ユースケース                │ │  アダプタ                   │
│   agent/loop_runner.rs       │ │   llm/openai.rs             │
│   agent/dispatch.rs          │ │   llm/anthropic.rs          │
│   agent/prompt.rs            │ │   llm/routing.rs            │
│   agent/session.rs           │ │   llm/retry.rs              │
│   tools/file/*.rs            │ │   llm/http.rs（共有配管）   │
│   tools/registry.rs          │ │   llm/sse.rs（SSE 枠組）    │
│                              │ │   fs/local.rs               │
│                              │ │   fs/search.rs              │
│                              │ │   fs/context.rs             │
│  ※ tokio に依存しない        │ │   config/{kinds,env}.rs     │
└───────────────┬──────────────┘ └─────────┬───────────────────┘
                │                          │
        ┌───────▼──────────────────────────▼───────┐
        │ agent-domain                             │
        │   model/   エンティティ・値オブジェクト  │
        │   ports/   trait（外界との境界）         │
        │   text.rs  共有カーネル（文字列の切詰め）│
        │   error.rs ドメインのエラー語彙          │
        │                                          │
        │   依存: std, serde, thiserror,           │
        │         async-trait, futures-core のみ   │
        └──────────────────────────────────────────┘
```

依存の向きは Cargo の依存関係でそのまま強制されます。
`agent-application` の `Cargo.toml` に `agent-infrastructure` は現れないので、
ユースケースが HTTP やファイルシステムを直接触ることは構造的に不可能です。

## 2. ポート一覧

| ポート | 実装 | 役割 |
|---|---|---|
| `LlmProvider` | `OpenAiCompatibleProvider`, `AnthropicProvider`, `RoutingProvider`, `RetryingProvider` | チャット補完 |
| `LlmRouter` | `StaticRouter`, `ModelPrefixRouter` | 委譲先の選択 |
| `Tool` | `ReadFileTool` ほか（application）, `TimeoutTool`（装飾） | モデルに公開する能力 |
| `FileSystem` | `LocalFileSystem` | サンドボックス化された I/O |
| `FileSearcher` | `IgnoreAwareSearcher` | `.gitignore` 準拠の内容検索 |
| `ContextProvider` | `WorkspaceContextProvider` | 環境情報とプロジェクト指示の収集 |
| `PromptBuilder` | `DefaultPromptBuilder`, `FixedPromptBuilder`, `AppendingPromptBuilder` | システムプロンプトの組み立て方針（既定・差し替え・追記） |
| `ApprovalGate` | `CliApprovalGate` | human-in-the-loop |
| `WebFetcher` | `GuardedWebFetcher` | 外向き HTTP（SSRF ガード・許可リスト・サイズ上限） |
| `CommandRunner` | `SandboxedCommandRunner` | 子プロセス実行（OS レベルの封じ込め・egress 許可リスト） |
| `EventSink` | `TerminalRenderer`, `NullEventSink` | 進捗の可観測性 |

すべて `dyn` 互換なので、合成ルートで実装を差し替えられます。
テストではこの差し替えがそのままフェイクの注入になります。

## 3. 設計上の判断とその理由

### 3.1 メッセージをブロック構造で持つ

`Message { role, content: Vec<ContentBlock> }` という Anthropic 寄りの表現を採用しています。
OpenAI 形式（`tool_calls` + `role:"tool"`）への写像は情報を落としませんが、逆は落ちるためです。

- OpenAI アダプタ … `Role::Tool` の各 `ToolResult` を個別の `role:"tool"` メッセージに展開
- Anthropic アダプタ … `Role::Tool` を `user` ターンの `tool_result` ブロックに変換し、
  同一ロールが連続する場合は 1 ターンにマージ（API が交互を期待するため）

### 3.2 サンドボックスをドメインに置く

「ワークスペースの外に出ない」はこのエージェントのビジネスルールであり、
アダプタの実装詳細ではありません。したがって `WorkspacePath` /
`WorkspaceRoot` はドメイン層にあり、**`WorkspacePath` を持たない限りファイルに触れない**
という型レベルの制約になっています。

ただし字句的な検査だけではシンボリックリンクを防げないため、
`LocalFileSystem` が canonicalize 後にもう一度ルート配下かを検査します
（ファイルシステムに触れられるのはインフラ層だけなので、この分担になります）。

### 3.2.1 子プロセスの封じ込めは OS から借りる

`WorkspacePath` の型レベル制約は**このプロセスの**ファイル操作にしか効きません。
`run_command` が起動する子プロセスはその外側にいるため、封じ込めは
オペレーティングシステムから来る必要があります。ポート
`CommandRunner::sandbox()` が返すのは*設定値ではなく実際に効いている封じ込め*で、
両者は食い違い得るからこそ、この区別を型で持たせています。

**外部依存ゼロ**を制約に選びました。この bin は CLI として、将来は SDK としても
配布するため、「動かす前に `apt install bubblewrap` が要る」サンドボックスは
実運用では無効になるのと同じです。Linux の Landlock は syscall、macOS の Seatbelt は
OS 同梱、egress プロキシはプロセス内 — いずれもインストール不要で成立します。

満たせない場合は**起動失敗**にしています。黙って弱い設定に落ちるサンドボックスは、
操作者が守られていると誤認する分、無いより危険だからです。

`SandboxKind` に全順序を入れていないのも同じ理由です。Seatbelt と Landlock は
プラットフォームごとの対等な機構であって強弱ではなく、順序を付けると
「Landlock 以上」という要求が macOS で起動失敗を引き起こします。要求は機構名ではなく
性質（`confined` / `isolated`）で表現します。

ドメイン許可リストにプロキシが必要なのは設計の好みではありません。Landlock の
ネットワークルールはポート単位で、カーネルの `landlock_net_port_attr` に宛先ホストの
フィールドが存在しません。宛先名を運べる唯一の手段が HTTP プロキシです。

### 3.3 ツール失敗は実行失敗ではない

未知のツール、引数不正、承認拒否、タイムアウト — いずれも
`is_error: true` の `tool_result` としてモデルに返り、ループは継続します。
モデルが自分の誤りから復帰するのは例外ではなく通常動作だからです。
実行を中断するのはプロバイダ自体が壊れている場合のみです。

同時に、**すべての `tool_call` に必ず 1 つの `tool_result` を返す**ことを
ディスパッチャが保証しています。対応が欠けると次のリクエストが
プロバイダに拒否されるためです。

### 3.4 tokio をユースケース層から追い出す

タイムアウトと再試行はタイマーを必要とします。これらを
`TimeoutTool` / `RetryingProvider` という**装飾子**としてインフラ層に置くことで、
`agent-application` はランタイム非依存のままです。
結果として `crates/application/tests/agent_loop.rs` は HTTP もファイルシステムも
タイマーも使わずにループ全体を検証できます。

### 3.5 ルータを最初から挟んでおく

プロバイダが 1 つでも `RoutingProvider` を経由します。コストは
1 リクエストあたり map 参照 1 回で、代わりに「2 つ目のモデルを足す」が
**コード変更ではなく設定変更**になります。

判断材料は `RequestMetadata` としてリクエストに同行します。

```rust
pub struct RequestMetadata {
    pub session_id: String,
    pub iteration: u32,
    pub task_kind: TaskKind,      // Agentic | Chat | Summarize | Classify
    pub requires_tools: bool,
    pub hints: BTreeMap<String, String>,
}
```

これにより、将来のルータはプロンプト文字列を解析せずに
「ツール不要の要約は安いモデルへ」といった判断ができます。

### 3.6 ツールディスパッチをループから切り出す

`ToolDispatcher`（`agent/dispatch.rs`）はループとは別の不変条件を持つため独立させています。

1. **すべての呼び出しに必ず結果を返す** — 返し損ねると次のリクエストがプロバイダに拒否される
2. **順序を保つ** — 並列実行したものも要求順に並べ直す
3. **並列化するのは読み取り系だけ** — 同一ターンの 2 つの書き込みは同じファイルを触りうる

ループ側は `dispatcher.dispatch(&calls).await` の 1 行になり、
承認・並列化・出力切り詰めの詳細を知りません。

### 3.7 設定を `EnvSource` 越しに読む

`Settings::from_source(&dyn EnvSource)` が本体で、`from_env()` はその薄いラッパです。
プロセス環境はグローバルな可変状態なので、直接読むと設定パースのテストが
並列実行できず互いに干渉します。`MapEnv` を注入すれば
「別名は大文字化される」「空文字は未設定扱い」「ベンダ既定のキー変数にフォールバックする」
といった規則を 1 つずつ固定できます。

### 3.8 出力チャネルの分離

ループは何も出力しません。`AgentEvent` を発行し、描画方法は
`EventSink` の実装が決めます。CLI ではモデルの散文を stdout、
ツール活動を stderr に出すため、`agent run "..." > answer.md` が期待どおりに動きます。
将来 Web UI や JSON ログを足す場合も、ループには手を入れません。

## 4. コンテキストエンジニアリング

システムプロンプトは 1 実行につき 1 回だけ組み立てられ、以下を含みます。

1. 環境情報（ルート、OS、日付、git かどうか）
2. ワークスペース概要（深さ 2 / 最大 60 件）
3. ツール一覧（説明の 1 行目のみ。全文はツール定義側にある）
4. 作業ルール 9 項目
5. プロジェクト指示ファイル（最大 8 KB で切り詰め）

ツール一覧の順序は `BTreeMap` で安定させています。プロンプトが実行ごとに揺れると
プロバイダ側のプロンプトキャッシュが効かなくなるためです。

履歴は毎イテレーション前に `Conversation::trim_to_budget` で刈り込まれ、
末尾 `keep_recent_messages` 件は必ず残ります。

## 5. テスト戦略

| 層 | 手法 | 例 |
|---|---|---|
| domain | 純粋な単体テスト | パス脱出の拒否、履歴刈り込みの不変条件、文字境界 |
| application | 全ポートをフェイク | ループの分岐、並列実行の順序保証、承認拒否 |
| infrastructure | `MapEnv` を注入 | 別名の大文字化、空文字の扱い、キーのフォールバック |
| infrastructure | ペイロード写像の単体テスト | `finish_reason:"stop"` + `tool_calls` の解釈 |
| infrastructure | 生 TCP のモックサーバ | ステータス写像、実際に送出される JSON |
| 全体 | E2E（`crates/cli/tests/end_to_end.rs`） | HTTP → ループ → 実ファイル書き込み |

モックサーバは `crates/test-support` に置いて infrastructure と cli の
両テストスイートで共有しています（HTTP フレームワークを足さずに済ませるため、
実装は生 TCP の約 60 行です）。

外部依存はゼロなので、`make test` はネットワークもモデルも要りません（計 188 本）。
