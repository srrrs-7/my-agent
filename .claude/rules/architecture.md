# アーキテクチャ規約

「どこに何を書くか」の判断基準。設計判断の**背景**は `management/docs/architecture.md` にあります
（そちらはリポジトリの読者向け、こちらは変更を加える側向け）。

## 層とその責務

| crate | 責務 | 許可される依存 |
|---|---|---|
| `agent-domain` | エンティティ、値オブジェクト、ポート(trait)、エラー語彙、共有カーネル(`text`) | std, serde, serde_json, thiserror, async-trait, futures-core(Stream trait のみ) |
| `agent-application` | ユースケース: ループ、ディスパッチャ、プロンプト組み立て、ツール実装 | domain, async-trait, futures, serde, tracing |
| `agent-infrastructure` | アダプタ: LLM の HTTP クライアント、FS、検索、設定、テレメトリ | domain, reqwest, tokio, futures, bytes, ignore, regex, chrono |
| `agent-cli` | 引数解析、描画、承認 UI、合成ルート | 全て |
| `agent-test-support` | テストダブル | tokio, serde_json |

`agent-application` に `tokio` を足さないでください（`invariants.md` §1）。

## 置き場所の判断

| 追加するもの | 置き場所 |
|---|---|
| 新しい概念・不変条件を持つ値 | `domain/src/model/` |
| 外界との新しい境界 | `domain/src/ports/` + `infrastructure` に実装 + `composition.rs` に結線 |
| モデルに公開する新しい能力 | `application/src/tools/` （`/add-tool` スキル参照） |
| 新しい LLM ベンダ | `infrastructure/src/llm/` （`/add-provider` スキル参照） |
| 横断的関心事（タイムアウト・リトライ・計測） | `infrastructure` の装飾子。ポートを実装して内側をラップする |
| 表示・整形 | `cli/src/render.rs` または `cli/src/commands/` |
| 複数層で使う純粋関数 | `domain/src/text.rs`（共有カーネル）。増やす前に本当に共有かを疑う |

## ポートと実装の対応

| ポート | 実装 |
|---|---|
| `LlmProvider` | `OpenAiCompatibleProvider`, `AnthropicProvider`, `RoutingProvider`, `RetryingProvider` |
| `LlmRouter` | `StaticRouter`, `ModelPrefixRouter` |
| `Tool` | `application/src/tools/file/*`（実装）, `TimeoutTool`（装飾） |
| `FileSystem` | `LocalFileSystem` |
| `FileSearcher` | `IgnoreAwareSearcher` |
| `ContextProvider` | `WorkspaceContextProvider` |
| `PromptBuilder` | `DefaultPromptBuilder`（外部注入のプロンプトは実装追加で対応） |
| `ApprovalGate` | `CliApprovalGate` |
| `EventSink` | `TerminalRenderer`, `NullEventSink` |

`LlmProvider` が合成パターンになっている点が重要です。ルータもリトライもそれ自体が `LlmProvider` なので、
呼び出し側は積み方の変化を認識しません。

```
Arc<dyn LlmProvider>
  = RetryingProvider(RoutingProvider({ "local": OpenAi…, "cloud": Anthropic… }))
```

将来のルーティング（コスト・レイテンシ・能力ベース、フェイルオーバー、A/B）は
`LlmRouter` の実装追加だけで足ります。判断材料の `RequestMetadata`
（`task_kind` / `iteration` / `requires_tools` / 任意の `hints`）は全リクエストに同行しています。
**プロンプト文字列を解析して分岐するルータを書かないでください** — その材料は既にあります。

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

## 設定

`Settings::from_source(&dyn EnvSource)` が本体、`from_env()` は薄いラッパです。
新しい設定項目を足すときは 3 箇所を同時に更新します。

1. `infrastructure/src/config/` — 読み取りと既定値
2. `.env.example` — コメント付きで記載
3. `README.md` §8 — 変数表

`make exec CMD="grep -rho 'AGENT_[A-Z_]*' crates/ | sort -u"` で実装との差分を確認できます。
