# my-agent

Rust 製の CLI コーディングエージェント。LLM に **prompt / context / tools** を渡し、
ツール呼び出しのループを回してワークスペース内のファイルを操作します。

- **Clean Architecture** — 依存は常に内向き（`cli → application / infrastructure → domain`）
- **コンテナファースト開発** — `cargo` は必ずコンテナ内で実行。ホストで依存を展開・実行しない
- **接続先は環境変数だけで切り替え** — Ollama などのローカル LLM とクラウド LLM の両対応
- **LLM ルーティングを織り込み済み** — ルータ自体が `LlmProvider` なので、呼び出し側は無変更

```
$ make setup
$ make ollama-up && make ollama-pull MODEL=qwen3:8b   # ローカル LLM を使う場合
$ make doctor
$ make ask Q="crates/domain の構成を説明して"
$ make chat
```

---

## 1. なぜコンテナの中で動かすのか

`cargo build` は crates.io から取得したコードの `build.rs` と proc-macro を**その場で実行**します。
npm の Shai-Hulud 型サプライチェーン攻撃と同じ経路が Rust にも存在するため、
このリポジトリでは開発時のランタイム操作をすべて使い捨てコンテナに閉じ込めます。

| 対策 | 実装箇所 |
|---|---|
| ホストに Rust ツールチェインを置かない | `docker/Dockerfile`, `Makefile` |
| 非 root 実行（ホスト UID/GID に一致） | `compose.yml` の `user:` |
| Linux capability を全 drop・特権昇格禁止 | `compose.yml` の `cap_drop` / `security_opt` |
| docker socket を**マウントしない** | `compose.yml`（意図的に不在） |
| ビルド／依存キャッシュを bind mount の外へ | named volume `cargo-registry` / `cargo-target` |
| 実行ごとにコンテナを破棄 | `docker compose run --rm`（全 make ターゲット） |

エージェント自身のファイル操作も、`AGENT_WORKSPACE` 配下に閉じ込められます（後述）。

---

## 2. セットアップ

必要なのは Docker だけです（ホストに Rust は不要）。

```bash
make setup     # .env を作成し、開発イメージをビルド
$EDITOR .env   # 接続先 LLM を設定
make doctor    # 設定の確認 + 疎通チェック
```

`make` は `docker compose` / `docker-compose` を自動判別します。

### ローカル LLM（Ollama）で動かす

```bash
make ollama-up                      # compose で ollama を起動
make ollama-pull MODEL=qwen3:8b     # モデルを取得
# .env:
#   AGENT_PROVIDER=openai
#   AGENT_BASE_URL=http://ollama:11434/v1
#   AGENT_MODEL=qwen3:8b
```

ホスト側で起動済みの Ollama / LM Studio を使う場合は
`AGENT_BASE_URL=http://host.docker.internal:11434/v1` を指定します。

### クラウド LLM で動かす

```bash
# OpenAI 互換（OpenAI / OpenRouter / Groq / Together / vLLM ...）
AGENT_PROVIDER=openai
AGENT_BASE_URL=https://api.openai.com/v1
AGENT_MODEL=gpt-4.1
AGENT_API_KEY=sk-...

# Anthropic
AGENT_PROVIDER=anthropic
AGENT_MODEL=claude-sonnet-5
AGENT_API_KEY=sk-ant-...
```

`openai` は OpenAI の `/chat/completions` プロトコルを話す**すべて**の実装を指します。
Ollama もこれに含まれるため、ローカルとクラウドで実装は共通です。

---

## 3. 使い方

```bash
make chat                      # 対話セッション（履歴保持）
make ask Q="..."               # 単発の質問
make tools                     # モデルに渡しているツール一覧
make doctor                    # 設定表示 + 疎通確認
make run ARGS="-v run 'hi'"    # 任意の引数で CLI を実行
```

`chat` 内のコマンド: `/reset` `/usage` `/tools` `/help` `/exit`

出力のチャネル分離:

- **stdout** … モデルの回答のみ（`agent run "..." > answer.md` がそのまま使えます）
- **stderr** … ツール実行ログ、承認プロンプト、警告

終了コード: `0` 正常終了 / `2` 反復上限に達して未完了 / `1` エラー。

### ツール承認（human-in-the-loop）

`AGENT_APPROVAL` で制御します。

| 値 | 挙動 |
|---|---|
| `auto` | 確認しない（コンテナ内での利用を想定） |
| `read-only`（既定） | 読み取り系は自動、書き込み系のみ確認 |
| `ask` | すべて確認 |

確認プロンプトでは `y` / `n` / `a`（以降すべて許可）に加え、**自由文で理由を返せます**。
拒否はエラーではなくツール結果としてモデルに戻るので、モデルは別の手段を提案できます。

---

## 4. エージェントに渡しているもの

### tools

| ツール | 種別 | 内容 |
|---|---|---|
| `read_file` | read-only | 行番号付きで読む。`offset` / `limit` でページング |
| `list_directory` | read-only | 1 階層のリスト |
| `search_files` | read-only | 正規表現検索（`.gitignore` 準拠・バイナリ除外） |
| `write_file` | mutating | 全文書き込み（親ディレクトリ自動作成） |
| `edit_file` | mutating | 完全一致の部分置換。一致 0 件／複数件はエラー |

`edit_file` を曖昧一致にしないのは意図的です。モデルが `read_file` で見た文字列と
一致しない限り編集は成立せず、意図しない箇所への適用が構造的に起こりません。

### context

システムプロンプトに毎回注入されるもの:

- ワークスペースの絶対パス、OS、当日日付、git リポジトリかどうか
- 深さ 2・最大 60 件の**ディレクトリ概要**（最初の 1 ターンを探索に使わせないため）
- **プロジェクト指示ファイル** — `AGENTS.md` → `CLAUDE.md` → `.agent/instructions.md` →
  `.github/copilot-instructions.md` の順で最初に見つかったもの
- ツール一覧と作業ルール

### loop

```
1. 履歴をコンテキスト予算まで刈り込む
2. モデルに問い合わせ（system + 履歴 + ツール定義）
3. tool_call が無ければ回答して終了
4. 各呼び出しを承認ゲートに通す
5. 実行（read-only は並列 / 書き込みは要求順に直列）
6. 結果を履歴に追加して 1 へ
```

安全弁として、反復上限（`AGENT_MAX_ITERATIONS`）、ツール出力の切り詰め
（`AGENT_MAX_TOOL_OUTPUT_BYTES`）、ツール単位のタイムアウト（`AGENT_TOOL_TIMEOUT_SECS`）、
履歴の予算管理（`AGENT_MAX_HISTORY_BYTES`）が入っています。

履歴の刈り込みは `tool_result` を対応する `tool_call` から切り離さないよう保証します
（切り離すと次のリクエストがプロバイダに拒否されるため）。

---

## 5. サンドボックス

ファイルツールは `AGENT_WORKSPACE` の外に出られません。防御は 2 段です。

1. **字句レベル** — `WorkspaceRoot::resolve` が `..` による脱出と範囲外の絶対パスを拒否
   （ドメイン層のビジネスルールとして実装）
2. **正規化レベル** — `LocalFileSystem` が canonicalize 後に再度ルート配下かを検査。
   ワークスペース内から外部を指すシンボリックリンクもここで弾かれます

いずれも `crates/domain/src/model/workspace.rs` と
`crates/infrastructure/src/fs/local.rs` のテストで固定しています。

---

## 6. アーキテクチャ

```
crates/
  domain/          エンティティ・値オブジェクト・ポート（trait）。std + serde のみ
  application/     ユースケース: エージェントループ、ツールディスパッチ、ツール実装
  infrastructure/  アダプタ: LLM の HTTP クライアント、ファイルシステム、設定、ログ
  cli/             プレゼンテーション + 合成ルート（DI はここだけ）
  test-support/    統合テスト用のモック LLM サーバ（dev-dependency のみ）
```

依存の向きは Cargo の依存関係でそのまま強制されます。
`application` は `infrastructure` を**知りません**。tokio にも依存していないため、
ループはランタイム抜きでテストできます（`crates/application/tests/agent_loop.rs`）。

詳細は [management/docs/architecture.md](management/docs/architecture.md) を参照してください。

### LLM ルーティング

ルータは合成パターンで、それ自体が `LlmProvider` です。

```
Arc<dyn LlmProvider>
  = RetryingProvider(         ← 指数バックオフ（Retry-After 尊重）
      RoutingProvider(        ← リクエストごとに委譲先を選択
        { "local": OpenAiCompatibleProvider,
          "cloud": AnthropicProvider }))
```

複数プロバイダは環境変数だけで定義できます。

```bash
AGENT_PROVIDERS=local,cloud
AGENT_DEFAULT_PROVIDER=local
AGENT_ROUTER=model-prefix

AGENT_PROVIDER_LOCAL_KIND=openai
AGENT_PROVIDER_LOCAL_BASE_URL=http://ollama:11434/v1
AGENT_PROVIDER_LOCAL_MODEL=qwen3:8b

AGENT_PROVIDER_CLOUD_KIND=anthropic
AGENT_PROVIDER_CLOUD_MODEL=claude-sonnet-5
AGENT_PROVIDER_CLOUD_API_KEY=sk-ant-...
```

この設定なら `make ask Q="..." ARGS="-m cloud/claude-sonnet-5"` のように
モデル参照でプロバイダを選べます。

将来コスト／レイテンシ／能力ベースのルーティングを足す場合も、
`LlmRouter` を実装するだけで済みます。判断材料として `RequestMetadata`
（`task_kind`, `iteration`, `requires_tools`, 任意の `hints`）が
すべてのリクエストに同行しています。呼び出し側の変更は不要です。

---

## 7. 開発

```bash
make build       # cargo build
make test        # 全テスト
make lint        # clippy -D warnings
make fmt         # rustfmt
make check       # fmt-check + lint + test（CI と同じ）
make audit       # cargo-audit で脆弱性スキャン
make shell       # コンテナ内のシェル
make cargo CMD="add --package agent-cli indicatif"
make exec  CMD="ls -la /target"
make clean-all   # コンテナ・イメージ・キャッシュボリュームを削除
```

テストは 129 本、外部ネットワークもモデルも不要です。

- ドメイン／ユースケース … フェイクのポートのみ
- 設定 … `EnvSource` 経由でインメモリ環境を注入（プロセス環境を汚さない）
- LLM クライアント … 生 TCP のモックサーバで実 HTTP を検証
- E2E … HTTP → provider → routing → retry → loop → tools → 実ファイルシステム
  （`crates/cli/tests/end_to_end.rs`）

---

## 8. 設定リファレンス

すべて `.env.example` にコメント付きで載っています。以下が全変数です。

### 接続先

| 変数 | 既定値 | 意味 |
|---|---|---|
| `AGENT_WORKSPACE` | カレントディレクトリ | サンドボックスのルート |
| `AGENT_PROVIDER` | `openai` | `openai` \| `anthropic` |
| `AGENT_BASE_URL` | `http://localhost:11434/v1` | エンドポイント。**この既定値はコンテナ外で直接実行した場合のもの**です。コンテナ内では `localhost` はコンテナ自身を指すため、compose の Ollama なら `http://ollama:11434/v1`、ホスト側の Ollama なら `http://host.docker.internal:11434/v1` を指定します（`make env` が作る `.env` には前者が入っています） |
| `AGENT_MODEL` | （必須） | モデル名 |
| `AGENT_API_KEY` | — | 無ければ `OPENAI_API_KEY` / `ANTHROPIC_API_KEY` |
| `AGENT_OPENAI_MAX_TOKENS_FIELD` | `max_tokens` | 新しい OpenAI モデル向けに `max_completion_tokens` へ変更可 |

### 複数プロバイダとルーティング（§6 参照）

| 変数 | 既定値 | 意味 |
|---|---|---|
| `AGENT_PROVIDERS` | — | 別名のカンマ区切り。設定すると単一プロバイダ設定より優先 |
| `AGENT_DEFAULT_PROVIDER` | 最初の別名 | 既定の委譲先 |
| `AGENT_PROVIDER_<別名>_KIND` | `openai` | 別名ごとの種別 |
| `AGENT_PROVIDER_<別名>_BASE_URL` | 種別の既定値 | 別名ごとのエンドポイント |
| `AGENT_PROVIDER_<別名>_MODEL` | （必須） | 別名ごとのモデル |
| `AGENT_PROVIDER_<別名>_API_KEY` | — | 別名ごとの API キー |
| `AGENT_ROUTER` | 単一なら `static`、複数なら `model-prefix` | `static` \| `model-prefix` |

### ループの安全弁（§4 参照）

| 変数 | 既定値 | 意味 |
|---|---|---|
| `AGENT_APPROVAL` | `read-only` | `auto` \| `read-only` \| `ask` |
| `AGENT_MAX_ITERATIONS` | `25` | 1 ターンあたりのモデル往復上限 |
| `AGENT_MAX_TOOL_OUTPUT_BYTES` | `32768` | 履歴に入るツール出力の上限 |
| `AGENT_MAX_HISTORY_BYTES` | `262144` | 履歴の予算。超過分は古い順に破棄 |
| `AGENT_TOOL_TIMEOUT_SECS` | `60` | ツール 1 回あたりの上限時間 |
| `AGENT_MAX_FILE_BYTES` | `2097152` | `read_file` が読む最大サイズ |
| `AGENT_PARALLEL_READ_TOOLS` | `true` | 読み取り系ツールの並列実行 |

### 生成とログ

| 変数 | 既定値 | 意味 |
|---|---|---|
| `AGENT_MAX_TOKENS` | `4096` | 生成トークン上限 |
| `AGENT_TEMPERATURE` | `0.2` | |
| `AGENT_REQUEST_TIMEOUT_SECS` | `180` | HTTP リクエストの上限時間 |
| `AGENT_MAX_RETRIES` | `3` | 一時障害の再試行回数 |
| `RUST_LOG` | `warn` | tracing フィルタ |
| `RUST_BACKTRACE` | `0` | |

---

今後の計画は [`management/issues/`](management/issues/) で管理しています。
