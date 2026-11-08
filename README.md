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
$EDITOR .env   # 接続先 LLM を設定（下の 3 つ）
make doctor    # 設定の確認 + 疎通チェック
```

`make` は `docker compose` / `docker-compose` を自動判別します。

設定が必須なのは **`AGENT_PROVIDER` / `AGENT_API_KEY` / `AGENT_MODEL` の 3 つだけ**です。
接続先（`AGENT_BASE_URL`）は `AGENT_PROVIDER` から決まり、それ以外の設定にはすべて既定値があります
（一覧は §8）。プロバイダとモデルには既定値がないので、必ず指定してください
（`AGENT_PROVIDER` が未設定なら、取りうる値を示して起動時に失敗します）。

```bash
# Anthropic
AGENT_PROVIDER=anthropic
AGENT_API_KEY=sk-ant-...
AGENT_MODEL=claude-sonnet-5-5

# OpenAI
AGENT_PROVIDER=openai
AGENT_API_KEY=sk-...
AGENT_MODEL=<OpenAI のモデル ID>
```

`AGENT_API_KEY` が無ければ `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` を読みます。
Anthropic と OpenAI はキーが無いと起動時に失敗します（最初のリクエストで 401 になるのを待たない）。

### ローカル LLM（Ollama）で動かす

```bash
make ollama-up                      # compose で ollama を起動
make ollama-pull MODEL=qwen3:8b     # モデルを取得
# .env:
#   AGENT_PROVIDER=ollama
#   AGENT_MODEL=qwen3:8b
```

キーは不要です。接続先は compose の ollama サービス（`http://ollama:11434/v1`）になります。
`ollama` では `OPENAI_API_KEY` を読みません（OpenAI のキーを Ollama の宛先に送らないため）。
認証付きのプロキシ越しなどでキーが要る場合だけ、`AGENT_API_KEY` を明示してください。
ホスト側で起動済みの Ollama / LM Studio を使う場合は
`AGENT_BASE_URL=http://host.docker.internal:11434/v1` を足します。

### その他の OpenAI 互換サーバ（OpenRouter / Groq / Together / vLLM ...）

```bash
AGENT_PROVIDER=openai
AGENT_BASE_URL=https://openrouter.ai/api/v1
AGENT_MODEL=...
AGENT_API_KEY=...        # キーを取らないサーバ（vLLM など）なら省略可
```

`openai` は OpenAI の `/chat/completions` プロトコルを話す**すべて**の実装を指します。
Ollama もこれに含まれるため、ローカルとクラウドで実装は共通です。
`AGENT_BASE_URL` を明示すると、どのプロバイダでも常にそちらが優先されます。

> **既定の接続先が変わりました。** 以前は `AGENT_PROVIDER=openai` で `AGENT_BASE_URL` を省くと
> `http://localhost:11434/v1`（Ollama）に繋ぎに行きましたが、今は OpenAI 本体
> （`https://api.openai.com/v1`）に繋ぎます。コンテナ内の `localhost` はコンテナ自身なので
> 元々どこにも繋がらなかったはずですが、Ollama を使うつもりだった場合は
> `AGENT_PROVIDER=ollama` に変えてください。
>
> **`AGENT_PROVIDER` は必須になりました。** 以前は未設定だと Ollama 扱いでしたが、今は起動時に
> 失敗します。`AGENT_MODEL` だけで Ollama を使っていた場合は `AGENT_PROVIDER=ollama` を足してください
> （複数プロバイダ形式の `AGENT_PROVIDER_<別名>_KIND` も同様です）。

---

## 3. 使い方

```bash
make chat                      # 対話セッション（履歴保持）
make ask Q="..."               # 単発の質問
make tools                     # モデルに渡しているツール一覧
make doctor                    # 設定表示 + 疎通確認
make eval TASKS=evals/tasks.json > report.json  # タスク集合で評価（下の「eval」）
make run ARGS="-v run 'hi'"    # 任意の引数で CLI を実行
```

`chat` 内のコマンド: `/reset` `/usage` `/plan` `/tools` `/help` `/exit`

**Ctrl-C** は「今どこにいるか」で意味が変わります（押す速さではなく状態で決まります）。

| どこで | Ctrl-C の意味 |
|---|---|
| `chat` のプロンプト待ち | 終了（Ctrl-D と同じく終了コード `0`） |
| ターンの実行中（モデルの応答待ち・ストリーミング・ツール実行・承認プロンプト） | **そのターンだけを止めてプロンプトに戻る**。会話は残る |
| 止めている最中にもう一度 | ターンの後始末を待たずに終了（`130`） |
| `agent run` の実行中 | ターンを止めて終了（`130`。送信中の通知は待ってから終わる） |

止めたターンの途中経過は会話にもセッションログにも残り、`--resume` でも戻ります。
結果を返す前だったツール呼び出しには「ユーザーが中断した（実行していない／途中まで反映されたかもしれない）」
という結果が付くので、次の入力でモデルはそれを踏まえて続けられます。ストリーミング途中の応答は履歴に残しません。
実行中の `run_command` はプロセスグループごと（孫プロセスを含めて）止まります。
承認プロンプトの途中で止めても、次に打った 1 行はチャットのプロンプトにそのまま届きます。
中断は通知しません（押した本人が端末の前にいるため）。

システムプロンプト（日付・ディレクトリ概要・`AGENTS.md` 等）はセッションの最初に 1 回だけ組み立てて固定します
（プロンプトキャッシュを効かせるため）。セッション中に `AGENTS.md` を編集した・ファイル構成が大きく変わった
といった場合は `/reset` で新しいセッションを始めると反映されます。

### 中断したセッションを再開する

会話はセッションログに残るので（§4）、そこから続きを始められます。

```bash
make run ARGS="sessions"                  # 保存されているセッション一覧（新しい順、サイズつき）
make run ARGS="chat --resume"             # 直近のセッションの続きから
make run ARGS="chat --resume <ID>"        # 特定のセッションの続きから
make run ARGS="sessions --delete <ID>"    # 記録を消す
```

再開に失敗しても**起動はします**。記録がない・壊れている・`--resume` を打ったが
まだ何も記録されていない、いずれの場合も理由を stderr に 1 行出して新規セッションで始まります。

再開したセッションは**記録の全部**を読み込みます（送信直前に切り詰められた形ではなく）。
予算を超えていれば最初のターンで通常どおり圧縮されるので、中断しなかった場合と同じ状態になります。
`AGENT_SESSION_LOG=false` で走らせたセッションは記録がないので再開できません。

応答は既定でストリーミング表示されます（生成と同時に流れる）。`AGENT_STREAM=false` で
従来の一括表示に戻せます。どちらでも stdout に出る内容は同一です。

出力のチャネル分離:

- **stdout** … モデルの回答のみ（`agent run "..." > answer.md` がそのまま使えます）
- **stderr** … ツール実行ログ、承認プロンプト、警告

終了コード: `0` 正常終了 / `2` 反復上限に達して未完了 / `1` エラー / `130` Ctrl-C で中断。

### ツール承認（human-in-the-loop）

`AGENT_APPROVAL` で制御します。

| 値 | 挙動 |
|---|---|
| `auto` | 確認しない（コンテナ内での利用を想定） |
| `read-only`（既定） | 読み取り系は自動、書き込み系のみ確認 |
| `ask` | すべて確認 |

確認プロンプトでは `y` / `n` / `a`（以降すべて許可）に加え、**自由文で理由を返せます**。
拒否はエラーではなくツール結果としてモデルに戻るので、モデルは別の手段を提案できます。

**依存の変更は、`auto` でも `a` の後でも確認します。** `write_file` / `edit_file` が
`Cargo.toml` や `package-lock.json` などの依存ファイルの中身を変えるときは、通常の確認とは別に、
追加・削除・変更される依存（名前と、バージョン・`git`・`package =` などの指定）を並べた専用の
プロンプトが出ます。

| 状況 | 依存の変更 |
|---|---|
| 対話（`chat` / 端末からの `run`） | ポリシーにも `a` にも関係なく必ず確認。**Enter だけは拒否**（`y` で許可） |
| 非対話（`agent run` を CI などで、`--yes` / `auto` を含む） | 拒否。ファイルは変わらず、理由がモデルに返る |
| `AGENT_ALLOW_DEPENDENCY_CHANGES=true` | 確認せず許可（無人実行で操作者が事前に決めた場合の逃げ道） |

`read-only` / `ask` では、同じ呼び出しに通常の確認と依存の確認の 2 回が出ることがあります。
仕組みと限界は §5「依存ファイルの変更」を参照してください。

### 通知（Webhook）

離席中に承認待ちで止まっていないか、終わったかを、スマホやチャットで受け取れます。
`AGENT_NOTIFY_WEBHOOK_URL` を設定すると、次のときに Webhook へ POST します（未設定なら何も送りません）。

| 種別 | いつ | 本文 |
|---|---|---|
| `approval` | 確認プロンプトを**実際に出した**とき（`auto` や非対話の自動拒否では送らない） | ツール名と安全区分のみ。引数は載せない |
| `completed` | 回答が出たとき。所要時間が `AGENT_NOTIFY_MIN_SECS` 未満なら送らない | 所要時間と、回答の冒頭（既定 100 文字） |
| `failed` | プロバイダのエラー、反復上限、プロバイダによる打ち切り | 所要時間と大まかな理由（エラー全文は端末にだけ出る） |

所要時間は「最後に人が操作した時点」から測ります。プロンプトを入力した時点が起点で、
承認プロンプトに答えるたびに更新されます。承認待ちで 1 時間放置したあとの短いターンは
「長いターン」として通知されません。

送信形式は `AGENT_NOTIFY_FORMAT` で `ntfy` / `slack` / `json` から選びます。
ntfy は JSON publish（サーバのルートへ `topic` を本文に入れて POST）なので、
LAN 内や localhost の自前 ntfy サーバでもそのまま使えます。

```bash
AGENT_NOTIFY_WEBHOOK_URL=https://ntfy.sh/<推測されにくい長いトピック名>
AGENT_NOTIFY_FORMAT=ntfy
```

送信は非同期で、失敗やタイムアウトは stderr に警告 1 行を出すだけです。エージェントの動作も
終了コードも変わりません。`agent run` は終了前に送信の完了を最大数秒待ちます。

> **本文は外部のサーバに出ていきます。** ターン完了の通知には回答の冒頭が載り、回答には
> モデルが読んだファイルの内容が含まれうります（プロンプトインジェクションで秘密を回答に
> 書かせれば、そのまま Webhook 先へ届きます）。回答を一切載せたくなければ
> `AGENT_NOTIFY_EXCERPT_CHARS=0` にしてください。文字数上限はこの経路の帯域制限も兼ねています。
>
> 抜粋中の URL は**わざと壊して**送ります（`https://` → `https:⁄⁄`）。Slack のリンク展開（unfurl）で
> 相手のサーバが URL を取得したり、通知からリンクを踏まされたりしないためです。Slack 形式では展開自体も無効にしています。
>
> **ntfy.sh の公開トピックは、トピック名を知っていれば誰でも購読できます。**
> 推測されにくい長いトピック名を使うか、自前の ntfy サーバを立ててください。
>
> Webhook URL は秘密情報として扱います（Slack の URL や ntfy のトピック名はそれ自体が認証情報です）。
> `agent doctor` やログには `scheme://host/…` だけが表示されます。

### タスク集合で評価する（eval）

プロンプト・モデル・ループの設定を変えたとき、良くなったかを**数字で**比べるためのコマンドです。
タスクの集合を実 LLM で流し、成功率・反復回数・トークン・レイテンシを JSON で出します。

```bash
make eval TASKS=evals/tasks.json > before.json             # 変更前
make eval TASKS=evals/tasks.json ARGS="--repeat 3" > after.json
agent eval evals/tasks.json --repeat 3 --out .agent/eval --keep > report.json
```

| フラグ | 意味 |
|---|---|
| `--repeat N` | 各タスクを N 回流す（既定 1）。実 LLM の結果はばらつくので、比べるなら 3 回以上を推奨 |
| `--out DIR` | 会話ログの置き場所。`<DIR>/sessions/` に試行ごとに 1 ファイル（既定 `.agent/eval`） |
| `--keep` | 試行ごとの作業ディレクトリを消さずに残し、レポートの `scratch_dir` に場所を書く（準備に失敗した試行も含む） |
| `--allow-mcp` | `AGENT_MCP_CONFIG` の MCP サーバを起動する（既定では起動しない） |
| `--allow-web` | `AGENT_WEB_FETCH=true` のとき `web_fetch` を渡す（既定では渡さない） |

> **信頼できないスイートを実行しないでください。** タスクファイルの `command` 検査は、書かれたコマンドを
> そのまま実行します（サンドボックス内ですが、任意のコマンド実行と同じです）。他人のタスクファイルは
> 中身を読んでから流してください。

グローバルな `--model` / `--max-iterations` / `--system-prompt-file` / `--append-system-prompt` も効きます。

**タスクファイル**は JSON 1 文書です。`fixture` と `prompt_file` はタスクファイルのあるディレクトリからの相対パスです。

```json
{
  "version": 1,
  "tasks": [
    {
      "id": "write-hello",
      "fixture": "fixtures/empty",
      "prompt": "hello.txt を作り、hello と書いてください。",
      "max_iterations": 5,
      "checks": [
        { "type": "file_contains", "path": "hello.txt", "contains": "hello" }
      ]
    },
    {
      "id": "fix-parser",
      "fixture": "fixtures/parser",
      "prompt_file": "prompts/fix-parser.md",
      "checks": [
        { "type": "file_contains", "path": "src/parser.rs", "contains": "fn parse" },
        { "type": "command", "run": "cargo test --offline", "timeout_secs": 300 }
      ]
    }
  ]
}
```

| 検査 | 合格の条件 |
|---|---|
| `file_contains` | 実行後のワークスペースで `path` のファイルが `contains` を部分文字列として含む。`path` はワークスペース内に限る（`..` や絶対パスは読み込み時に拒否） |
| `command` | `run` をワークスペース直下で実行し、終了コードが 0（`timeout_secs` 既定 120） |

読み込みは厳格です。未知のフィールド、検査が 0 件のタスク、重複した `id`、`prompt` と `prompt_file` の
両方または一方もないタスクは、**モデルを 1 回も呼ばずに**エラーで止まります（打ち間違いで検査が
黙って「常に合格」になるのを防ぐため）。`id` は英数字・`-`・`_` のみです。

各試行は次のように動きます。

- `fixture` を一時ディレクトリ（`$TMPDIR/agent-eval-*`）に複製し、そこをワークスペースにして
  `agent run` と同じ合成ルートでエージェントを組み立てる。**実ワークスペースもフィクスチャも変更しません**。
  フィクスチャ内のシンボリックリンクは拒否します（複製先から外を指せてしまうため）
- 承認は `auto`（使い捨てのコピーなので）。ただし依存ファイルの変更は従来どおり別に問われ、
  答える人がいないため拒否されます（`AGENT_ALLOW_DEPENDENCY_CHANGES=true` で通す）。Webhook 通知は送りません
- 「使い捨てのコピーしか触らない」はファイル系ツールにしか成り立たないため、外部に作用するツールは
  明示しない限り渡しません。MCP サーバは `--allow-mcp`、`web_fetch` は `--allow-web` を付けたときだけです
  （`run_command` は従来どおり、サンドボックス必須で渡します）
- 試行の準備（フィクスチャの複製、エージェントの組み立て、検査用サンドボックスの起動）に失敗した場合は、
  その試行を `finish_reason: "error"` として記録し、残りの試行を続けます。フィクスチャ内の
  シンボリックリンクや特殊ファイルは、全タスク分を**最初の LLM 呼び出しより前に**検査します
- `command` 検査は `run_command` と同じサンドボックスで、試行ごとの一時ディレクトリをルートに実行します
  （`cargo test` はモデルが書いた `build.rs` を実行するため）。`AGENT_SHELL_SANDBOX=none` でも検査は
  封じ込めを要求し、得られない環境（macOS のホストなど）では**最初の LLM 呼び出しより前に**失敗します
- `AGENT_SHELL=true` でも、`AGENT_SHELL_SANDBOX=none` のときは eval では `run_command` をモデルに渡しません

stdout はレポートの JSON だけです。進捗（`[1/4] write-hello #1 PASS ...`）と失敗した検査の理由は stderr に出ます
（端末を操作する制御文字は `?` に置き換えます）。検査の出力をレポートに入れる前に、既知の秘密の値は
`[REDACTED]` に置き換えます（§5「秘密の値の伏せ字」と同じ対象）。

**Ctrl-C** は `agent run` と同じです。1 回目で実行中の試行を中断し（その試行は `finish_reason: "interrupted"`、
不合格、検査は実行しません）、それまでの試行のレポートを `"interrupted": true` 付きで出力して 130 で終了します。
2 回目は試行の終了を待たずに放棄します。どちらでも作業ディレクトリは片付けます（`--keep` を除く）。

**終了コード**

| コード | 意味 |
|---|---|
| `0` | 全タスクの全試行が合格 |
| `1` | 実行できなかった。スイート全体（タスクファイル不正、フィクスチャがない・リンクを含む、設定不正、検査用サンドボックスが得られない — この場合レポートは出ません）、または一部の試行（プロバイダの障害、試行の準備失敗 — レポートは出ます） |
| `3` | 全試行を実行でき、合格しなかった試行がある（成功率を比べてよいのはこの場合と `0` だけ） |
| `130` | Ctrl-C で中断した（それまでの試行のレポートは出ます） |

**レポートの形**（`schema` はフィールドの意味が変わったときに上がります。追加では上がりません）

```jsonc
{
  "schema": 1,
  "interrupted": false,                   // Ctrl-C で途中までのレポートなら true
  "fingerprint": {
    "agent_version": "0.1.0",
    "provider": "default", "model": "claude-sonnet-5",
    "tools": ["edit_file", "glob", "..."],
    "run_command_sandbox": null,          // run_command を渡したときの実際の封じ込め
    "check_sandbox": "landlock",          // command 検査の封じ込め（検査がなければ null）
    "settings": { "max_iterations": 25, "max_history_bytes": 200000, "compact": true,
                  "tool_timeout_secs": 60, "allow_mcp": false, "allow_web": false,
                  "global_instructions_fnv1a": null, "...": "..." },
    "system_prompt_fnv1a": "9f3c2a7b0e1d4c55"
  },
  "summary": {
    "tasks": 2, "repeat": 3,
    "attempts": 6, "passed": 5, "pass_rate": 0.8333,
    "iterations": 31, "mean_iterations": 5.17,
    "tokens": { "input": 52000, "output": 4100, "cache_read": 0, "cache_write": 0, "total": 56100 },
    "mean_tokens": 9350.0,
    "model_latency_ms": 48210, "wall_ms": 61377,
    "compactions": 0, "trims": 0, "tool_calls": 22, "tool_errors": 1, "denials": 0
  },
  "tasks": [
    {
      "id": "write-hello",
      "attempts": 3, "passed": 3, "pass_rate": 1.0, "...": "summary と同じ集計",
      "runs": [
        {
          "attempt": 1, "passed": true, "finish_reason": "completed",
          "iterations": 2, "tokens": { "...": "..." }, "model_requests": 2,
          "model_latency_ms": 1830, "wall_ms": 2104,
          "compactions": 0, "trims": 0, "tool_calls": 1, "tool_errors": 0, "denials": 0,
          "session_id": "write-hello-1-<uuid>",
          "checks": [ { "type": "file_contains", "target": "hello.txt", "passed": true, "detail": "found" } ]
        }
      ]
    }
  ]
}
```

- **合格**は「実行がエラーで終わらず、全検査が通った」こと。`finish_reason` が `max_iterations` でも、
  検査が通っていれば合格です（仕事は済んでいるため）。プロバイダが壊れた試行と準備に失敗した試行は
  `finish_reason: "error"` と `error` を持ち、不合格になります（残りのタスクは続行、終了コードは 1）
- `tokens` は圧縮の要約リクエストの分も含みます。`model_latency_ms` はループ本体のモデル応答待ちの合計、
  `wall_ms` は試行全体の経過時間です
- `system_prompt_fnv1a` は、システムプロンプトを**固定の文脈**（ワークスペースのパス・日付・ディレクトリ概要を
  プレースホルダにしたもの）で描画した結果の FNV-1a です。テンプレート・`--system-prompt-file`・
  `--append-system-prompt`・渡すツールが変わると変わり、日付やフィクスチャでは変わりません。
  `AGENT_GLOBAL_INSTRUCTIONS` の中身はこの固定の文脈に入らないため、`settings.global_instructions_fnv1a`
  に別に載せます（未設定なら `null`）

**2 回分を比べる**には、まず `fingerprint` が意図した差分だけかを確かめ、次に `summary` を並べます。

```bash
jq -s '[.[] | {prompt: .fingerprint.system_prompt_fnv1a, model: .fingerprint.model}]' before.json after.json
jq -s '[.[] | .summary | {pass_rate, mean_iterations, mean_tokens, wall_ms}]' before.json after.json
jq -s '[.[0].tasks, .[1].tasks] | transpose | map({id: .[0].id, before: .[0].pass_rate, after: .[1].pass_rate})' \
  before.json after.json
```

---

## 4. エージェントに渡しているもの

### tools

| ツール | 種別 | 内容 |
|---|---|---|
| `read_file` | read-only | 行番号付きで読む。`offset` / `limit` でページング |
| `list_directory` | read-only | 1 階層のリスト |
| `search_files` | read-only | 正規表現検索（`.gitignore` 準拠・バイナリ除外） |
| `glob` | read-only | パスのパターン（`src/**/*.rs`）でファイルを探す。更新が新しい順・既定 100 件で打ち切り（打ち切ったことと総数を表示）。`.gitignore` 準拠 |
| `write_file` | mutating | 全文書き込み（親ディレクトリ自動作成） |
| `edit_file` | mutating | 完全一致の部分置換。一致 0 件／複数件はエラー |
| `todo_write` | read-only | 作業計画（手順と pending / in_progress / completed）を丸ごと書き換える。ワークスペースには触れない |
| `web_fetch` | network | URL の内容をテキスト取得（**既定で無効**。`AGENT_WEB_FETCH=true` で有効化） |
| `run_command` | destructive | シェルコマンドをサンドボックス内で実行（**既定で無効**。`AGENT_SHELL=true` で有効化） |
| `mcp__<サーバ>__<ツール>` | destructive | 設定した MCP サーバのツール（**既定で無効**。`AGENT_MCP_CONFIG` で有効化。§5） |

`edit_file` を曖昧一致にしないのは意図的です。モデルが `read_file` で見た文字列と
一致しない限り編集は成立せず、意図しない箇所への適用が構造的に起こりません。

`todo_write` は複数手順の作業でモデルが計画を書き、進むたびに更新するためのツールです。
更新のたびに stderr に現在の計画がチェックリストで表示され（stdout は汚れません）、
`chat` では `/plan` でいつでも見られます。ワークスペースを変えないので `read-only` に分類しており、
既定の承認ポリシーでも確認は入りません。毎回全体を置き換える意味なので、同じターンに複数回
呼ばれても（並列実行されても）最後に要求されたものが残ります。作業中は 1 つまで・20 項目まで・
各項目は 1 行 200 バイトまでで、違反は直さずにエラーとしてモデルに返します。

計画の実体は**会話そのもの**（成功した最後の `todo_write` の引数）で、別のファイルは持ちません。
そのため `chat --resume` では記録から計画が戻り、`/reset` で消えます。履歴が圧縮・刈り込みで
短くなり計画が見えなくなったときだけ、未完了の計画を「エージェントのメモ」として履歴の末尾に
1 回だけ追記します（システムプロンプトには入れないので、プロンプトキャッシュは崩れません）。

`web_fetch` は**既定で無効**です。URL はモデルが組み立てる外向きのメッセージであり、
クエリにワークスペースの内容を載せて送信できてしまうため、有効化は操作者の明示的な
判断（`AGENT_WEB_FETCH=true`）としています。有効化しても:

- プライベート帯・ループバック・リンクローカル（クラウドメタデータ 169.254.169.254 を含む）、
  `localhost`・ドットなしホスト名（`ollama` 等）・`host.docker.internal`・`.internal`/`.local`
  への接続は常に拒否されます（DNS 解決後の全アドレスも検査。リダイレクトも 1 ホップごとに再検査）
- 安全分類は `network` で、既定の承認ポリシーでは**毎回確認**が入り、承認プロンプトに
  URL 全体が表示されます（`AGENT_APPROVAL=auto` では確認が入らない点に注意）
- 取得内容は指示ではなくデータとしてツール結果でのみモデルに渡ります

`run_command` も**既定で無効**です。安全分類を `destructive` にしているのは、
コマンドが必ず破壊的だからではなく、**引数を読んでも破壊的かどうか判断できない**
からです。`cargo test` と `rm -rf /` は同じ形をしています。したがって承認プロンプトには
シェル行がそのまま表示され、人間がそれを読んで判断します。

封じ込めはコマンド文字列の検査ではなく OS が行います（§5）。許可リスト方式の
コマンド検査を採用しなかったのは、`make` のようなエントリが 1 つ入るだけで
`make exec CMD="..."` により任意実行と等価になり、リストが安全性を保証しないためです。

### context

システムプロンプトに毎回注入されるもの:

- ワークスペースの絶対パス、OS、当日日付、git リポジトリかどうか
- 深さ 2・最大 60 件の**ディレクトリ概要**（最初の 1 ターンを探索に使わせないため）
- **指示ファイル** — 一般的なものから具体的なものの順に連結します。並び順は優先順位ではありません。
  食い違ったときは**操作者のグローバル指示がリポジトリの指示より優先**し、サブディレクトリの
  指示ファイルはそのディレクトリでの補足にとどまる（ルートの指示とグローバル指示に反する内容には
  従わない）とプロンプトに明記します。リポジトリに置かれたファイルは誰でもコミットできるためです
  1. 操作者のグローバル指示（`AGENT_GLOBAL_INSTRUCTIONS` に絶対パスで指定。未設定なら無し）
  2. ワークスペース直下の指示ファイル — `AGENTS.md` → `CLAUDE.md` → `.agent/instructions.md` →
     `.github/copilot-instructions.md` の順で最初に見つかった 1 つ
  3. サブディレクトリの指示ファイルは**中身を読み込まず、パスだけを一覧**します（深さ 3 まで・
     最大 20 件。`.gitignore` 対象と隠しディレクトリは除外。ディレクトリごとに上と同じ順で 1 つ）。
     モデルはそのディレクトリで作業する前に `read_file` で読みます。モノレポの全 `AGENTS.md` を
     最初から積むとタスクに無関係な部分でプロンプトが埋まり、またプロンプトはセッション中
     固定なので、作業の進行に合わせて差し込むこともできないためです

  1 と 2 は合わせて 16 KiB までで、超えた分は切り詰めたことと元の大きさを本文中に明記します
  （両方あるときグローバル側は半分まで）。ワークスペース内の指示ファイルは `read_file` と同じ
  アクセス検査（§5）を通り、拒否されたファイルは読み込みも一覧もしません。ディレクトリ内で
  最初の候補が拒否された場合、次の候補には進みません（拒否されるファイルを手前に置いて
  読み込むファイルを選ばせないため）。グローバル指示は操作者自身が指定したファイルなので、
  ワークスペースの外にあってよく、アクセス検査の対象外です
- ツール一覧と作業ルール

システムプロンプトは**セッションの最初の 1 回だけ**組み立て、以後のターンでは同じものを送ります。
エージェント自身がファイルを書くたびにディレクトリ概要が変わり、プロンプトのバイト列が動くと、
プロバイダ側のプロンプトキャッシュ（前置きのバイト一致で効く）が会話全体で外れるためです。
書いたものは会話履歴に残っているので、モデルが見失うことはありません。
概要を取り直したいときは `chat` で `/reset` します（`--resume` で再開したセッションも組み立て直します）。

システムプロンプトは外部から注入できます。`AGENT_SYSTEM_PROMPT_FILE`（または
`--system-prompt-file`）で全体を差し替え、`AGENT_APPEND_SYSTEM_PROMPT`（または
`--append-system-prompt`）で末尾に追記します。CLI フラグが環境変数より優先されます。
差し替えてもツール定義はプロンプトとは別に渡るため、ツール呼び出しは機能し続けます。
サンドボックスはコードで強制されており、プロンプトの差し替えで弱まることはありません。

### loop

```
1. 履歴をコンテキスト予算に収める（圧縮 → 刈り込み）
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

### 履歴の圧縮

履歴が予算を超えたとき、いきなり削除するのではなく、**古いターンを LLM が書いた要約 1 通に
畳みます**（`AGENT_COMPACT`、既定で有効）。削除が最初に消すのは最も古いメッセージであり、
最も古いメッセージとは**あなたが何を頼んだかを書いた行**だからです。直近の依頼を覚えたまま
当初の要件を忘れたエージェントは、要件を覚えていて細部を忘れたエージェントより役に立ちません。

段階は 2 つで、順序に意味があります。

1. **圧縮** — 直近のやり取りをそのまま残し、それ以前を要約に置き換える
2. **刈り込み** — それでも予算に収まらない分を削除する。予算は絶対で、最後は誰かが消える

### 新鮮度の重み付けは「経過時間」ではなく「メッセージの順序と大きさ」で決まります

そのまま残す範囲は**通数と容量の両方**で頭打ちになります（`AGENT_COMPACT_KEEP_RECENT` と
`AGENT_COMPACT_KEEP_RECENT_BYTES`）。通数だけでは尺度になりません — 12 通は
一行の返答なら 2 KB、ツール結果なら 384 KB で、後者は `AGENT_MAX_HISTORY_BYTES` を
単独で超えます。守るはずだった直近のやり取りが、書いたばかりの要約を予算から押し出すわけです。
**何通残すかは、その通が実際どれだけ大きかったかで決まります。**

要約に畳む側も、境目でばっさり切れるのではなく**段階的に薄くなります**。1 ブロックが
要約器に渡せるバイト数は、後ろに下がるほど半分ずつになり（8 メッセージごと）、下限で止まります。
直近の続きに使われやすい手前のターンは詳細のまま、底の方は輪郭だけが渡ります。

**ユーザーの発言だけは、どれだけ古くても切り詰められません。** セッション全体が答えている
当の依頼だからです。前回の要約は user ロールで入るので、この保護がそのまま効きます
（効かなければ、圧縮のたびに前回の圧縮を打ち消すことになります）。

判断材料は**経過時間ではありません**。効くのは「それ以降どれだけ会話が進んだか」であって、
何時間経ったかではない。3 時間放置しただけのセッションは何も先に進んでいないのに、
時計はそれを「古い」と報告してしまいます。

### 畳まれた原文はログに残ります

圧縮も刈り込みも**元に戻せません**。要約は要点を残しますが原文は返ってこないし、
刈り込みに至っては要約すら残しません。そこで会話は、**履歴が短くされる前に**
1 行 1 JSON で追記されます（`AGENT_SESSION_LOG`、既定で有効、既定の場所は
`.agent/sessions/<session-id>.jsonl`）。

```bash
# あのとき何を読んだのか
jq -r 'select(.role=="tool") | .content[].content' .agent/sessions/*.jsonl
```

- 追記専用なので、途中でプロセスが落ちても最後の完全な行までは読めます
- 各行にスキーマ版が入っています
- 書き込みに失敗してもターンは止まりません（警告して続行）
- 進捗表示（`EventSink`）とは別物です。あちらはツール結果を 1 行に要約するので、
  記録には使えません

> **注意: このファイルは会話をそのまま持ちます。**モデルが読んだファイルの中身も全部です。
> 既定ではワークスペース内に置かれます。ファイルツールからは読めませんが（§5）、
> `run_command` を有効にしていると**コマンドからは読めます**。気になる場合は `AGENT_SESSION_DIR` にワークスペース外の絶対パスを指定するか、
> `AGENT_SESSION_LOG=false` で切ってください。

古い記録は起動時に自動で消えます。**直近 50 セッション、かつ 30 日以内**が既定です
（`AGENT_SESSION_KEEP` / `AGENT_SESSION_MAX_AGE_DAYS`。どちらかを超えたら消えます）。

- `0` は上限の解除であって「全部消す」ではありません。両方 `0` で永久保存です
- **`--resume` で名指ししたセッションは、どれだけ古くても消えません**
- 消えたときは stderr に 1 行出ます。現在の設定は `agent doctor` の `session retention` で確認できます
- 消すのはディレクトリ内のセッション**数と古さ**です。1 つのファイルが際限なく太る件
  （同じセッションを何度も再開した場合）はまだ対処していません

要約プロンプトは散文ではなく**作業状態の引き継ぎ**を要求します（決定と*その理由*、
試して駄目だった方法、変更したファイルと正確な識別子、そして**ユーザーの全発言**）。
理由の落ちた決定は蒸し返され、失敗の記録がなければ同じ失敗を繰り返すからです。

- 追加コストは 1 リクエストで、**内容が失われる瞬間にしか発生しません**
- リクエストは `TaskKind::Summarize` を運ぶので、ルータが安いモデルへ回せます
- 要約が失敗しても従来の削除に落ちるだけで、ターンは止まりません
- 要約は `user` ロールで履歴の先頭に入ります。プロバイダは会話が user で始まることを
  期待し、assistant で入れるとモデルが「自分が言ったこと」として扱ってしまうためです

---

## 5. サンドボックス

ファイルツールは `AGENT_WORKSPACE` の外に出られません。防御は 2 段です。

1. **字句レベル** — `WorkspaceRoot::resolve` が `..` による脱出と範囲外の絶対パスを拒否
   （ドメイン層のビジネスルールとして実装）
2. **正規化レベル** — `LocalFileSystem` が canonicalize 後に再度ルート配下かを検査。
   ワークスペース内から外部を指すシンボリックリンクもここで弾かれます

いずれも `crates/domain/src/model/workspace.rs` と
`crates/infrastructure/src/fs/local.rs` のテストで固定しています。

### ワークスペース内の保護ファイル

ワークスペースの中にも、プロジェクトのファイルではないものがあります。資格情報、
エージェント自身の会話ログ、git の管理情報です。ファイルツールはこれらに触れません。
`.gitignore` は検索から外すだけで読み取りは止めないため、別の規則として持っています。

| 対象 | 読み取り | 書き込み |
|---|---|---|
| `.env`, `.env.*`（`.env.example` を除く）, `*.pem`, `*.key`, `id_rsa*`, `id_ed25519*`, `.netrc`, `.npmrc`, `.pypirc` | 拒否 | 拒否 |
| セッションログのディレクトリ（`AGENT_SESSION_DIR` がワークスペース内のとき。既定 `.agent/sessions/`） | 拒否 | 拒否 |
| `.git/`（`.git` という名前のファイルを含む） | 可 | 拒否 |
| `AGENT_DENY_READ` で追加したもの | 拒否 | 拒否 |
| `AGENT_DENY_WRITE` で追加したもの | 可 | 拒否 |

- **パターン** — `/` を途中に含まない名前（`*` は 1 個まで）は、どの階層のファイル名・
  ディレクトリ名にも一致し、一致したディレクトリの配下もすべて対象です。途中に `/` を含む
  パターン（`config/master.key`、`deploy/secrets/`）はワークスペース直下からの相対パスです。
  大文字小文字は区別しません（`.ENV` も拒否）。`?` `[...]` `**` `!` は起動エラーにします。
  別の意味に読まれて、黙って何も守らない規則になるのを防ぐためです
- **Unicode の別表記も同じ名前として扱います** — macOS の APFS（と、その Docker bind mount）は
  Unicode の大文字小文字畳み込みと正規化（NFD）を経て名前を比べるため、`x.\u{212A}ey`
  （ケルビン記号）で `x.key` が開きます。ASCII に畳まれうる文字（`K` `ſ` `ß` `ﬁ` など。
  NTFS 向けに `ı` も）は ASCII に読み替えてから照合します。ASCII で書いた規則にはこれで漏れが
  ありません。ASCII 以外の文字で書いた規則は大文字小文字だけを同一視し、合成済み／分解済みの
  違い（`é` と `e` + 結合アクセント）は同一視しないので、必要なら両方の綴りを列挙してください
- **既定の規則は外せません** — 設定でできるのは追加だけです
- **2 段で検査します** — モデルが指定した名前と、シンボリックリンクを解決した実体の両方を
  見ます。`notes -> .env` も `.env -> config/app.env` も拒否されます。リンク先が存在しない
  シンボリックリンクは辿りません（書き込みがリンク先を作るのを防ぐため）
- **`.env` は書き込みも拒否します。** 書き換えられると、モデルが `AGENT_BASE_URL` を
  自分の用意したサーバに向け、次回の起動で API キーがそこへ送られます
- `search_files` は拒否対象の中身を返しません（`include` で明示しても同じです）。
  `glob` は拒否対象をパスとしても一覧に出しません（`**/.env*` と明示しても同じです）。
  どちらもシンボリックリンクは辿らず、一致としても返しません。
  `list_directory` は名前だけなので表示します
- 起動時に読む指示ファイル（`AGENTS.md` など）も同じ検査を通します。リポジトリが
  `AGENTS.md -> .env` を置いても、システムプロンプトには入りません
- 拒否はツールの失敗として返り、ループは続きます。モデルには「ユーザーに尋ねること」
  「別の手段で取りにいかないこと」を伝えます

有効な規則は `agent doctor` の `file access` 行に 1 行で表示されます。

#### 秘密の値の伏せ字

上の規則はファイルを守るもので、値そのものは守りません。そこで、このプロセスが
知っている秘密の値を、ツールの出力（本文・要約・エラー文）と通知の抜粋から
`[REDACTED]` に置き換えます。

- 全プロバイダの API キー
- Webhook URL（URL 全体、パスとクエリ、ntfy のトピック名）
- MCP サーバに渡す環境変数の値（`env` と `env_from` の両方）
- 子プロセスから除去される名前（`*_API_KEY` / `*_TOKEN` / `*_SECRET` など。
  `run_command` の環境変数除去と同じ判定）の環境変数の値。`.env` は起動時に
  プロセス環境へ読み込まれるため（§8 の拒否対象を除く）、そこに書かれた他の資格情報もここに含まれます

8 文字未満の値は、普通の文字列と衝突するため対象外です。
最後の項目（名前から推測した値）はさらに条件を絞り、**12 文字以上で、かつ英小文字・英大文字・
数字・記号のうち 2 種類以上を含む値**だけを伏せます。`POSTGRES_PASSWORD=postgres` の
`postgres` のような辞書語を伏せると、`read_file` の出力の普通の単語まで `[REDACTED]` になり、
モデルがそれを書き戻してファイルを壊すためです。操作者が設定した値（上の 3 項目）は
この条件を受けず、8 文字以上なら伏せます。`SSH_AUTH_SOCK` / `GPG_AGENT_INFO` のような
ソケットの場所は子プロセスからは除きますが、秘密ではないので伏せ字の対象にしません。

#### 残るリスク

- **`run_command` は `.env` を読めます。** Landlock は読み取りを広く許可しており
  （下記）、部分木だけを除外できません。`cat .env` の出力に既知の値があれば伏せ字に
  なりますが、それ以外の値（プロセスが知らない秘密）はそのまま出ます。
  同じ理由でセッションログも `run_command` からは読めます
- 伏せ字は**完全一致のみ**です。Base64 などに符号化された値、分割された値、
  出力の切り詰めで途中が切れた値の断片は通ります
- ハードリンクは別名の同じファイルです。パスで判定する規則では見分けられません
  （作れるのは `run_command` かリポジトリ自身です）
- `run_command` がファイルツールの検査と実際の読み書きの間にシンボリックリンクを
  差し替える競合は防げません
- 検査はこのプロセスのファイル操作だけに効きます。`run_command` で git を実行すれば
  `.git/` に書けます（下記の `.git` の制限を参照）

### 子プロセスのサンドボックス

上記 2 段はこのプロセス自身のファイル操作にしか効きません。`run_command` が起動する
子プロセスは `WorkspacePath` の外にいるため、封じ込めは OS から来る必要があります。

**外部依存ゼロ**を設計上の制約にしています。この bin は CLI として配布し、将来は SDK
としても配布するため、「動かす前に `apt install` が要る」サンドボックスは
「実際には無効なサンドボックス」と同義になるからです。

| OS | 機構 | 状態 |
|---|---|---|
| Linux 6.7+ | Landlock（syscall。インストール不要、非特権で自己適用） | 実装済み |
| macOS | Seatbelt（OS 同梱） | 未実装（`AGENT_SHELL=true` で起動失敗） |
| Windows | — | 非対応。WSL2 で実行してください |

Landlock 層で実際に強制されるもの（`crates/infrastructure/tests/sandbox.rs` が
**本物の子プロセスを起動して**固定しています）:

- 書き込みはワークスペース＋セッション専用一時ディレクトリ＋操作者が明示した
  追加ルートのみ。共有 `/tmp` は読み取り専用
- 読み取りは広く許可。ツールチェインは全域を読むため、実用に耐える読み取り許可リストは
  結局「ほぼ全部」になります。資格情報は環境変数の除去と下記の egress 許可リストで守ります
- `*_API_KEY` / `*_TOKEN` / `*_SECRET` 等の環境変数は子プロセスに渡しません。
  名前に関係なく、操作者が設定した秘密（API キー、Webhook URL）を値に含む変数も渡しません
- 外向き TCP は egress プロキシのポート以外すべて拒否。プロキシは
  ドメイン許可リスト（`AGENT_SHELL_NETWORK_ALLOW`）で `CONNECT` と絶対形式リクエストを
  検査します。許可リストが空なら**プロキシを起動せず、通信は一切できません**
- タイムアウト時も正常終了時も、プロセスグループごと SIGKILL（バックグラウンドの孫プロセスを残さない。
  `(sleep 20; echo x >> Cargo.toml) &` のように終了後に動く孫も、事後の依存検知の前に止まります）
- **残余リスク:** `setsid` / `setpgid` で自分のセッション・グループへ抜けたプロセスは
  グループへの SIGKILL が届かず、コマンドの終了後も生き残り得ます（サンドボックスの封じ込めは
  そのまま効きます）。塞ぐには PID 名前空間などが要り、今は対処していません

**この層の限界も明記します。** Landlock のルールはカーネルが *union* する純粋な
許可リストで、拒否も「より具体的なルールが勝つ」規則もありません。したがって
書き込み可能なワークスペースの中の `.git` だけを読み取り専用にすることは**表現できません**
（`landlock_cannot_protect_git_inside_the_writable_workspace` テストがこの制限自体を固定しています）。
また Landlock のネットワークルールは**ポート単位**で宛先アドレスを持たないため、
「プロキシのポート」に居る別サービスには到達し得ます。どちらも network namespace 層
（`SandboxKind::NetnsProxied`）で塞ぐべきもので、別 issue として管理しています。

`AGENT_SHELL_SANDBOX` が満たせない環境では**起動に失敗します**。黙って弱い設定に
フォールバックしないのは、静かに劣化するサンドボックスが最も危険だからです。

### 依存ファイルの変更

依存のマニフェストとロックファイルは、次のビルドがどの第三者のコードを取得して実行するかを
決めます。そこで、それらへの変更は他の編集と分けて扱います。**範囲は 2 つで、強さが違います。**

**ファイルツール経由（`write_file` / `edit_file`）— 事前に承認します。**
実行の直前（同じターンの先行する呼び出しがすべて終わった後）に、書き込み後の内容を計算し、
変わるファイルが依存ファイルなら専用の承認を求めます（§3）。

- 対象はファイル名で判定し、大文字小文字を区別しません（`cargo.toml` も `Cargo.toml` と同じ。
  macOS などでは同じファイルとして開かれるため）。Unicode の別表記
  （`package-lock.j\u{17F}on` など）も保護ファイルと同じ規則で同一視します。ディレクトリは問いません（`.cargo/` の
  `config` / `config.toml` だけは `.cargo` ディレクトリの中にあるときに限ります）
- **シンボリックリンクは解決してから判定します。** `deps.txt -> Cargo.toml` への書き込みは
  `Cargo.toml` への書き込みとして承認を求め、プロンプトには `deps.txt -> Cargo.toml` と出ます
- 対象: Cargo（`Cargo.toml` / `Cargo.lock`）、npm 系（`package.json` / `package-lock.json` /
  `npm-shrinkwrap.json` / `yarn.lock` / `pnpm-lock.yaml` / `bun.lock(b)`）、Python
  （`requirements*.txt` / `pyproject.toml` / `Pipfile(.lock)` / `poetry.lock` / `uv.lock`）、
  Go（`go.mod` / `go.sum` / `go.work` / `go.work.sum`）、Ruby（`Gemfile(.lock)`）、
  PHP（`composer.json` / `composer.lock`）
- **取得元を決める設定ファイルも対象です**（レジストリやインデックスの差し替えは依存の追加と
  同じく次のビルドが実行するコードを変えるため）: `.cargo/config.toml` / `.cargo/config`、
  `.yarnrc.yml` / `.yarnrc`、`.pnpmfile.cjs`、`bunfig.toml`、`pip.conf`、`uv.toml`。解析はせず、
  変わった行を見せます。`.npmrc` はアクセス方針で読み書きとも拒否済みのため対象外です
- 書き込み後の内容を計算できなかった場合（`old_string` の不一致、タイムアウト、I/O エラーなど）は、
  依存ファイルかどうか判定できないため**実行しません**。通常のツール失敗として表示され（拒否扱い
  ではありません）、モデルにはツール本来のエラーの後に「何も書いていない」旨の一文が返ります。
  引数そのものの誤り（`InvalidInput`）だけは実行に進み、ツール本来のエラーを返します
- 既存ファイルが読めない（大きすぎる・UTF-8 でない等）場合は「新規ファイル」ではなく
  「中身を表示できない既存ファイル」と表示し、承認は必ず挟みます
- 依存の一覧として読めるのは `Cargo.toml` / `Cargo.lock` / `package.json` / npm のロックファイル /
  `requirements*.txt` / `go.mod`。`[patch.*]`・`overrides`・`--extra-index-url`・npm の install 系
  スクリプトも一覧に出ます。TOML / YAML のパーサは足していません（パーサ自体が依存になるため）
- **確認するかどうかは「依存ファイルの中身が変わるか」だけで決まります。** 解析できない形式・
  解析に失敗した内容・依存の項目が変わらない編集（コメントなど）も、変わった行を見せて確認します。
  パーサの誤りや見落としで確認が飛ばされることはありません
- 表示は件数を区切り、制御文字・双方向制御文字・不可視文字を `?` に置き換えます
  （通常の承認プロンプトと stderr の表示も同じです）

**`run_command` 経由（`cargo add` / `npm install` など）— 事後に検知するだけです。**
コマンドが何を書くかは引数から分からないため、前後でワークスペースの依存ファイルを比較し、
変わっていればツール結果の**先頭**に注記してモデルに伝え、stderr に警告を出します。
**止めることはできません。** 比較する時点でコマンドは終わっており、`build.rs` なども既に実行されています。

- 監視するのは安全区分が `destructive` のツールすべてです（名前ではなく性質で決めます）
- `node_modules` / `target` / `.git` / `.venv` / `venv` / `__pycache__` / `.tox` と、
  `.gitignore` などで無視されたディレクトリの中は比較しません。`.gitignore` はディレクトリの
  枝刈りにだけ使い、ファイルには適用しません（無視されている `Cargo.lock` の変更も拾います）。
  `vendor` は比較します（取り込んだマニフェストの改ざんはサプライチェーンの経路のため）
- 走査は 1 回あたり 20,000 エントリで打ち切り、打ち切ったときはその旨を結果に書き、
  stderr にも警告を出します
- タイムアウトで打ち切られたコマンドも比較します。Ctrl-C で止めた場合は比較しません
- `AGENT_ALLOW_DEPENDENCY_CHANGES=true` でもこの検知は止まりません

**既知の穴（v1）:** ハードリンクで依存ファイルに別名を付けると、ファイルツールの判定は
その別名を依存ファイルとして扱いません（ハードリンクは同じファイルの別名で、パスからは
見分けられません）。`run_command` の事後検知は `.cargo/config(.toml)` をファイル名だけでは
判定できないため検知しません。既知の脆弱性データベース（OSV など）への照会は行いません（別 issue）。

### MCP サーバ（stdio）

操作者が設定した MCP サーバを起動し、そのツールを `mcp__<サーバ名>__<ツール名>` として
モデルに渡します。**既定で無効**で、`AGENT_MCP_CONFIG` に設定ファイルの**絶対パス**を
指定したときだけ動きます。**Linux（Landlock）限定**です。macOS では封じ込めが未実装のため、
設定すると起動に失敗します。

```json
{
  "servers": {
    "issues": {
      "command": "/usr/local/bin/issues-mcp",
      "args": ["--read-only"],
      "env": { "ISSUES_MODE": "readonly" },
      "env_from": ["ISSUES_TOKEN"],
      "allow_hosts": ["api.example.com"],
      "workspace_access": "none"
    }
  }
}
```

| キー | 既定値 | 意味 |
|---|---|---|
| `command` | （必須） | 起動するプログラムの**絶対パス**（PATH 検索はしない）。シンボリックリンクは解決してから検査する |
| `args` | `[]` | 引数 |
| `env` | `{}` | サーバに渡す環境変数（値を直接書く）。このプロセスの環境から秘密らしい変数を除いた**後で**設定する。**秘密でない値用**: 設定ファイルの中身は全サーバと `run_command` から読める（下記）。念のためツール出力からは伏せ字にする |
| `env_from` | `[]` | エージェント自身の環境から起動時にコピーする変数名。**秘密はこちらで渡す**（例: `.env` に `ISSUES_TOKEN=...` を書き、ここに `"ISSUES_TOKEN"`）。エージェントの環境に無い名前があれば**起動失敗**（変数名を表示）。値はツール出力から伏せ字にし、他のサーバと `run_command` の環境からは値で除去する。`env` と同じ名前は書けない |
| `allow_hosts` | `[]`（**通信不可**） | サーバ専用の egress プロキシが通すドメイン。空ならプロキシを起動せず全拒否 |
| `workspace_access` | `"none"` | `"none"`（書き込めるのはサーバ専用ディレクトリだけ）\| `"read-write"`（ワークスペースにも書ける） |

サーバ名は `[a-z0-9-]` の 1〜16 文字。未知のキーは無視せず**起動エラー**にします
（`workspace_acess` のような綴り間違いが黙って既定値に落ちないように）。上限はサーバ 8 個、
1 サーバあたりツール 64 個。

**信頼モデル — 設定ファイルは操作者のものです。** 設定はプログラムを起動する権限そのものなので:

- **設定ファイルは任意コード実行と同等です。** `"command": "/bin/sh", "args": ["-c", "..."]` と
  書けば何でも走ります。`AGENT_MCP_CONFIG` は**信頼できるファイルだけ**に向けてください

- 設定ファイルが**ワークスペースの中にあれば起動失敗**します。clone したリポジトリに同梱された
  設定で任意のコードが走ること、モデルが次回の設定を書き換えることを防ぐためです
- 起動するプログラム（`command`）がワークスペースの中にあっても起動失敗します。`args` の
  絶対パスがワークスペース内の**ファイル**を指す場合（`/usr/bin/node /workspace/server.js`）も
  同様です。ただしこれは発見的な検査で、環境変数や相対 import 経由でワークスペースのコードを
  読み込ませる形は検出できません
- サーバの作業ディレクトリは常にサーバ専用ディレクトリです（相対パスがワークスペースに解決されない）
- `npx` / `uvx` で起動するサーバは避け、**事前にインストールしたバイナリ**を指定してください。
  起動時のパッケージ取得は egress の既定（全拒否）と衝突し、取得そのものがサプライチェーンの
  経路になります（取得のために `registry.npmjs.org` を許可すると、サーバはそこへ任意に通信できます）

**封じ込められるもの**（`crates/infrastructure/tests/mcp_sandbox.rs` が本物のサーバプロセスで固定）:

- 書き込みはサーバ専用ディレクトリ（`TMPDIR` もここ）だけ。`workspace_access: "read-write"` の
  ときだけワークスペースが加わる。サーバは呼び出しの合間にも動けるため、承認ゲートも
  依存ファイルの検知も通らない書き込み（`Cargo.toml` や `.git/hooks`）を既定では許しません
- 外向き TCP はサーバ専用プロキシのポートのみ。プロキシは `allow_hosts` で名前を検査します
- 環境変数は `run_command` と同じく除去され、`env` と `env_from` のものだけが追加されます。
  `AGENT_MCP_CONFIG` 自体も（秘密ではないが設定ファイルの場所を教えるため）全サーバと
  `run_command` の環境から除去します
- サーバ専用ディレクトリは起動ごとに新しく作り（所有者のみ `0700`）、同名の既存ディレクトリは
  再利用しません
- エージェントの終了時に stdin を閉じ、プロセスグループごと SIGKILL します（同じグループの
  孫プロセスも残さない。`setsid` で抜けたものは下記）
- サンドボックスが得られなければ、サーバを起動せず**エージェント全体の起動に失敗**します。
  `AGENT_MCP_SANDBOX` は `AGENT_SHELL_SANDBOX` と別で、`none` は指定できません

**封じ込められないもの:**

- **読み取りは閉じられません。** `"none"` でもワークスペース全体・`.env`・セッションログ
  （既定でワークスペース内）・ホームディレクトリを読めます。読んだものを外へ出せるかは
  `allow_hosts` 次第です
- `env` / `env_from` で渡した値はそのサーバから読めます（渡す目的がそれなので）
- **設定ファイルに直接書いた `env` の値は、全サーバと `run_command` から読めます。**
  読み取りは閉じられず、設定ファイルの場所は推測できるためです（環境変数から除去しても
  ファイルそのものは読める）。秘密は `env_from` で渡してください。`env_from` の値は
  設定ファイルにも、そのサーバ以外の子プロセスの環境にも現れません。ただし読み取りが
  閉じられない以上、値の出どころ（ワークスペース内の `.env`、同じユーザーの
  `/proc/<エージェントの pid>/environ`）を読まれる経路は残ります
- **`setsid` で新しいセッションに移った孫プロセスは、プロセスグループごとの kill から漏れます。**
  確実に後始末するには subreaper か cgroup が必要で、まだ実装していません
- Landlock のネットワーク規則はポート単位のため、`run_command` と同じ「同じポートの別サービス」の
  穴が残ります（§5 子プロセスのサンドボックス）
- `read-write` のサーバによるワークスペースへの書き込みは、呼び出しの前後で依存ファイルを
  比較して検知するだけで（上記）、呼び出しの合間の書き込みは検知しません
- エージェントが SIGKILL されると、サーバは後始末されずに残ります

**モデルへの渡し方:**

- 安全区分は**一律 `destructive`** です。サーバが申告する `readOnlyHint` などは信用しません。
  既定の承認ポリシーでは毎回確認が入ります
- ツール名は使えない文字を `_` に置き換え 64 文字で切ります。呼び出しにはサーバの元の名前を使います。
  変換後の名前が組み込みツールや他のツールと**衝突したら起動失敗**します（黙って上書きしない）
- 説明文とスキーマは信頼できない入力として扱います。制御文字・不可視の書式文字（タグ文字・
  異体字セレクタを含む）を除き、説明は 1 行・1 KiB に切って先頭に `[MCP server <名前>]` を付け、
  スキーマは 16 KiB を上限とします。スキーマ内で 1 行に整形するのは `description` / `title` だけで、
  `enum` / `const` / `default` / `pattern` などの値は空白をそのまま残します（不可視文字のみ除去）。
  プロパティ名などのキーに制御文字・不可視文字があれば、名前を書き換えず**起動失敗**にします
- 結果はテキストのみ。画像などは「省略した」という印に置き換えます。`isError` はエラーとして返ります
- ツール一覧は起動時に 1 回取得して固定します（`list_changed` には追従しません）。
  クラッシュしたサーバは再起動せず、以後の呼び出しはエラーになります。1 メッセージの上限は 4 MiB。
  サーバからの要求には（`ping` 以外）「未対応」と答えますが、送信待ちが 64 件を超えるほど
  stdin を読まずに要求を送り続けるサーバは停止扱いにします。id が数値か 128 バイト以下の
  文字列でない要求には答えません
- 呼び出しは `AGENT_TOOL_TIMEOUT_SECS` で打ち切られ、サーバには `notifications/cancelled` を送ります

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
  = RoutingProvider(                          ← リクエストごとに委譲先を選択
      { "cloud": FailoverProvider(            ← 委譲先をまたぐ切り替え（AGENT_FAILOVER 設定時のみ）
                   RetryingProvider(Anthropic),        ← 委譲先ごとの指数バックオフ
                   [RetryingProvider(OpenAiCompatible)]),
        "local": RetryingProvider(OpenAiCompatible) })
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

#### ローカル優先のハイブリッド構成

履歴圧縮の要約（と将来の分類リクエスト）をローカルの Ollama に、エージェントループを
クラウドに送り、クラウドが落ちたらローカルで続ける構成も環境変数だけで組めます。

```bash
AGENT_PROVIDERS=local,cloud
AGENT_DEFAULT_PROVIDER=cloud
AGENT_ROUTE_SUMMARIZE=local      # これだけで AGENT_ROUTER=task-kind になる
AGENT_ROUTE_CLASSIFY=local
AGENT_FAILOVER=local             # cloud が落ちたら local へ
```

- **`task-kind` ルータ** — リクエストの `task_kind`（`agentic` / `chat` / `summarize` /
  `classify`）ごとに `AGENT_ROUTE_<種別>` の委譲先へ送ります。未設定の種別は
  `AGENT_DEFAULT_PROVIDER` へ。ただし `-m cloud/claude-sonnet-5` のような**明示的な
  プロバイダ指定が常に勝ちます**。指定はその実行全体に効くため、要約も含めて `cloud` へ
  行きます。接頭辞のない `-m` は既定プロバイダのモデル名として扱い、別の委譲先へ
  回ったリクエストではその委譲先の設定モデルに置き換えます
- **フェイルオーバー** — 委譲先がまず自分のリトライを使い切り、そのうえで接続失敗・
  タイムアウト・5xx・429 だったときだけ `AGENT_FAILOVER` の順に次を試します。
  第一候補の 401 / 400 などは設定の誤りなので切り替えずにそのまま失敗します（切り替えると誤りが
  隠れるため）。切り替え先が 401 / 400 を返した場合は警告を出して残りの候補を続けます。
  全滅したときに返るのは第一候補のエラーです（切り替え先の 401 で本当の障害を上書きしないため）。
  切り替え先には切り替え先自身の設定モデルを要求します
- **ストリームは途中で切り替えません** — 出力が始まった後の失敗はそのまま失敗として
  返ります。切り替えると、途中まで表示した回答に別の回答が継ぎ足されるためです
- **クールダウン** — 一度切り替わった委譲先は `AGENT_FAILOVER_COOLDOWN_SECS`（既定 60 秒）
  の間スキップし、直接切り替え先へ送ります。経過後は再び第一候補から試すので、
  復旧すれば自動で戻ります。クールダウン中でも切り替え先が全滅したら第一候補を最後に試します。
  経過後の最初の 1 回は**リトライなしの 1 回だけ**で確かめ（応答しない第一候補に
  毎回リトライ分の数分を払わないため）、失敗するたびにクールダウンを倍にします
  （60 → 120 → 240 秒…、上限 10 分。成功すれば設定値に戻ります）
- **能力** — 委譲先が公表する能力は第一候補のものです。ツールを使えない切り替え先は、
  ツールを要求するリクエストでは飛ばされます（ツールの無いローカルモデルを要約用の
  切り替え先にしても、エージェントループからツールが消えません）
- **見え方** — 切り替えは `-v` なしでも stderr に 1 行の警告が出ます（別名と大まかな理由のみ。
  URL やキーは出しません）。`-v` を付けるとリクエストごとの委譲先と理由が
  `agent::route` の行として出ます。要約リクエストもここで確認できます

> **注意: Ollama は文脈長を超えた入力を黙って切り詰めます。** Ollama の既定の文脈長
> （`num_ctx`）はモデルの最大値より小さいことが多く、超えた分はエラーにならず先頭から
> 捨てられます。要約をローカルに回す場合、要約に渡す転記が切り詰められると、
> 最も古い部分（＝ユーザーの最初の依頼）が要約から消えます。モデル側の文脈長を
> 十分に取るか、`AGENT_COMPACT_KEEP_RECENT_BYTES` / `AGENT_MAX_HISTORY_BYTES` を
> 小さめにしてください。文脈長の自動調整はこのエージェントでは行っていません。

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

テストは 890 本、外部ネットワークもモデルも不要です。

- ドメイン／ユースケース … フェイクのポートのみ
- 設定 … `EnvSource` 経由でインメモリ環境を注入（プロセス環境を汚さない）
- LLM クライアント … 生 TCP のモックサーバで実 HTTP を検証
- E2E … HTTP → provider → routing → retry → loop → tools → 実ファイルシステム
  （`crates/cli/tests/end_to_end.rs`。配線はテスト側で組み直した最小構成）
- 合成ルート … 本物の `composition::assemble` を `MapEnv` の設定で呼び、登録ツール・装飾子・
  承認ゲート・会話ログ・サンドボックス不成立時の起動失敗を固定（`crates/cli/tests/composition.rs`）
- 実バイナリ … `agent` を子プロセスとして起動し、モック LLM とモック Webhook だけを相手に
  合成ルートと終了処理まで通す（`crates/cli/tests/notify_e2e.rs`）。Ctrl-C は実際に SIGINT を送って
  終了コードと会話の保持を確かめる（`crates/cli/tests/interrupt_e2e.rs`）

---

## 8. 設定リファレンス

以下が全変数です。必須は `AGENT_PROVIDER` / `AGENT_API_KEY` / `AGENT_MODEL` の 3 つだけで
（Ollama ならキーも不要）、`.env.example` にはこの 3 つしか載せていません。
それ以外はすべて任意で、表の既定値で動きます。

値が空の行（`AGENT_API_KEY=`）は未設定として扱います。

### `.env` から設定できない変数

`.env` はカレントディレクトリから親へたどって見つけたものを読み、実環境変数にないキーだけを補います。
ただし**能力を広げる・封じ込めを緩める変数は `.env` からは設定できません**。見つけた場合は無視し、
名前を 1 行の警告で stderr に出します。実環境変数（`export`、`docker compose` の `env_file` を含む）
で渡したものは従来どおり効きます。

| 変数 | 理由 |
|---|---|
| `AGENT_MCP_CONFIG`, `AGENT_MCP_SANDBOX` | 任意のプログラムを起動できる |
| `AGENT_SHELL`, `AGENT_SHELL_SANDBOX`, `AGENT_SHELL_NETWORK_ALLOW`, `AGENT_SHELL_WRITABLE` | コマンド実行を有効にする・封じ込めを緩める |
| `AGENT_APPROVAL`, `AGENT_ALLOW_DEPENDENCY_CHANGES` | 人間の承認を省く |
| `AGENT_WEB_FETCH`, `AGENT_WEB_FETCH_ALLOW`, `AGENT_WEB_FETCH_ALLOW_PRIVATE`, `AGENT_NOTIFY_WEBHOOK_URL` | 外部へ送る経路を開く |
| `AGENT_GLOBAL_INSTRUCTIONS`, `AGENT_SYSTEM_PROMPT_FILE`, `AGENT_APPEND_SYSTEM_PROMPT` | モデルへの指示を差し替える |
| `AGENT_WORKSPACE` | ファイルツールの届く範囲を変える |
| `AGENT_SESSION_DIR` | 会話の原文（モデルが読んだファイルの中身を含む）の書き出し先と、保持期間による削除の対象を変える |
| `PATH`, `LD_PRELOAD`, `LD_LIBRARY_PATH`, `LD_AUDIT`, `DYLD_*`（3 種）, `BASH_ENV`, `ENV`, `HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`, `SSL_CERT_FILE`, `SSL_CERT_DIR`（大文字小文字を問わない） | 起動する子プロセスと HTTPS 通信（API キーを含む）の行き先を変える |

理由は、**リポジトリに同梱された `.env` による権限昇格を防ぐため**です。他人のリポジトリを clone して
その中で `agent` を起動しただけで、そこに置かれた `.env` が MCP サーバを起動したり、承認を `auto` に
したりできてはいけません。上の表にない変数（ループの設定など）は従来どおり `.env` に書けます。

**接続先は、API キーと同じ所からだけ読みます。** 実環境変数に API キーが 1 つでもあるとき、
`.env` の次の変数は無視し、名前を 1 行の警告で stderr に出します。

| 種別 | 変数 |
|---|---|
| API キー（実環境にあるかを見る） | `AGENT_API_KEY`, `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `AGENT_PROVIDER_<ALIAS>_API_KEY`（空の値は数えない） |
| 接続先（無視されるもの） | `AGENT_PROVIDER`, `AGENT_BASE_URL`, `AGENT_MODEL`, `AGENT_OPENAI_MAX_TOKENS_FIELD`, `AGENT_PROVIDERS`, `AGENT_DEFAULT_PROVIDER`, `AGENT_PROVIDER_*`（すべて）, `AGENT_ROUTER`, `AGENT_ROUTE_*`, `AGENT_FAILOVER*`（大文字小文字を問わない） |

理由は、**他人のリポジトリの `.env` に自分の API キーの送り先を決めさせないため**です。キーを
`export` しておき接続先は既定のまま使っているところへ、同梱の `.env` が `AGENT_BASE_URL` や
新しい別名（`AGENT_PROVIDERS` + `AGENT_PROVIDER_<ALIAS>_BASE_URL`。`openai` / `anthropic` の別名は
`OPENAI_API_KEY` / `ANTHROPIC_API_KEY` を読む）を書けば、最初のリクエストでキーがそのホストへ送られます。
モデル名は送り先を変えませんが、同じプロバイダ設定の組なので揃えて扱います。

キーも `.env` に書いている場合（`make setup` の想定する使い方）は、接続先も含めてすべて従来どおり効きます。
キーを実環境に置き、接続先を `.env` に書いていた場合は、接続先も実環境へ移してください
（あるいはキーも `.env` に書く）。

### 接続先

| 変数 | 既定値 | 意味 |
|---|---|---|
| `AGENT_PROVIDER` | （必須） | `openai` \| `ollama` \| `anthropic`（別名: `openai-compatible` / `compatible` → `openai`、`claude` → `anthropic`）。接続先の既定値を決める。未設定なら起動失敗 |
| `AGENT_API_KEY` | — | 無ければ `OPENAI_API_KEY`（`openai`）/ `ANTHROPIC_API_KEY`（`anthropic`）。`ollama` はベンダの変数を読まず、明示したときだけ送る。`anthropic` と、`AGENT_BASE_URL` を省いた `openai` では**必須**（無ければ起動失敗） |
| `AGENT_MODEL` | （必須） | モデル名。プロバイダごとの既定値は持たない |
| `AGENT_BASE_URL` | プロバイダで決まる（下表） | エンドポイント。明示すれば常にこちらが優先 |
| `AGENT_OPENAI_MAX_TOKENS_FIELD` | `max_tokens` | 新しい OpenAI モデル向けに `max_completion_tokens` へ変更可 |
| `AGENT_WORKSPACE` | カレントディレクトリ（compose 内は `/workspace`） | サンドボックスのルート |

`AGENT_BASE_URL` を省いたときの接続先:

| `AGENT_PROVIDER` | 既定の `AGENT_BASE_URL` |
|---|---|
| `openai` | `https://api.openai.com/v1` |
| `ollama` | `http://ollama:11434/v1`（compose の ollama サービス。`make ollama-up`） |
| `anthropic` | `https://api.anthropic.com` |

ホスト側の Ollama / LM Studio は `http://host.docker.internal:11434/v1`、コンテナを使わず
直接実行するなら `http://localhost:11434/v1` を明示します（コンテナ内の `localhost` はコンテナ自身です）。

### 複数プロバイダとルーティング（§6 参照）

| 変数 | 既定値 | 意味 |
|---|---|---|
| `AGENT_PROVIDERS` | — | 別名のカンマ区切り。設定すると単一プロバイダ設定より優先 |
| `AGENT_DEFAULT_PROVIDER` | 最初の別名 | 既定の委譲先 |
| `AGENT_PROVIDER_<別名>_KIND` | （必須） | 別名ごとのプロバイダ。値と接続先の既定値は `AGENT_PROVIDER` と同じ。未設定なら起動失敗 |
| `AGENT_PROVIDER_<別名>_BASE_URL` | プロバイダで決まる | 別名ごとのエンドポイント |
| `AGENT_PROVIDER_<別名>_MODEL` | （必須） | 別名ごとのモデル |
| `AGENT_PROVIDER_<別名>_API_KEY` | — | 別名ごとの API キー。フォールバックと必須の規則は `AGENT_API_KEY` と同じ |
| `AGENT_PROVIDER_<別名>_MAX_TOKENS_FIELD` | `max_tokens` | 別名ごとの `AGENT_OPENAI_MAX_TOKENS_FIELD` |
| `AGENT_ROUTER` | 単一なら `static`、複数なら `model-prefix`。`AGENT_ROUTE_*` があれば `task-kind` | `static` \| `model-prefix` \| `task-kind` |
| `AGENT_ROUTE_AGENTIC` / `_CHAT` / `_SUMMARIZE` / `_CLASSIFY` | — | `task-kind` ルータでの種別ごとの委譲先（別名）。未設定の種別は既定の委譲先。未知の別名や、`task-kind` 以外のルータと併用すると起動失敗 |
| `AGENT_FAILOVER` | — | 委譲先が落ちたときに順に試す別名のカンマ区切り。空なら切り替えなし。未知の別名は起動失敗 |
| `AGENT_FAILOVER_COOLDOWN_SECS` | `60` | 切り替えた委譲先をスキップする秒数。`0` なら毎回第一候補から試す |

### ループの安全弁（§4 参照）

| 変数 | 既定値 | 意味 |
|---|---|---|
| `AGENT_APPROVAL` | `read-only` | `auto`（確認なし。コンテナ内でのみ使う）\| `read-only`（読み取り系は確認なし、変更系は確認）\| `ask`（毎回確認） |
| `AGENT_ALLOW_DEPENDENCY_CHANGES` | `false` | `true` で依存ファイルの変更を確認せず許可（CI 用。`AGENT_APPROVAL` とは独立。`run_command` 後の検知は残る。§5） |
| `AGENT_MAX_ITERATIONS` | `25` | 1 ターンあたりのモデル往復上限 |
| `AGENT_MAX_TOOL_OUTPUT_BYTES` | `32768` | 履歴に入るツール出力の上限 |
| `AGENT_MAX_HISTORY_BYTES` | `262144` | 履歴の予算。超過分は圧縮、それでも溢れれば古い順に破棄 |
| `AGENT_COMPACT` | `true` | 溢れた履歴を要約に畳む（§4 参照）。`false` で従来どおり削除のみ |
| `AGENT_COMPACT_KEEP_RECENT` | `12` | 圧縮後もそのまま残す直近メッセージ数の上限 |
| `AGENT_COMPACT_KEEP_RECENT_BYTES` | `65536` | 同じくバイト数の上限。通常こちらが先に効く（§4 参照） |
| `AGENT_SESSION_LOG` | `true` | 会話を追記ログに残す（§4 参照）。会話全文がディスクに残る |
| `AGENT_SESSION_DIR` | `.agent/sessions` | ログの置き場所。相対パスは `AGENT_WORKSPACE` 起点。ワークスペース内ならファイルツールから読めない（§5）。ワークスペースのルートそのものは起動エラー |
| `AGENT_SESSION_KEEP` | `50` | 残すセッション数の上限。`0` で無制限 |
| `AGENT_SESSION_MAX_AGE_DAYS` | `30` | 残す日数の上限。`0` で無期限 |
| `AGENT_TOOL_TIMEOUT_SECS` | `60` | ツール 1 回あたりの上限時間 |
| `AGENT_MAX_FILE_BYTES` | `2097152` | `read_file` が読む最大サイズ |
| `AGENT_DENY_READ` | —（既定の規則のみ） | ファイルツールが読み書きしないパターンのカンマ区切り（例: `*.p12,secrets/`）。既定の規則（§5）に**追加**する。外す手段はない。解釈できないパターンは起動エラー |
| `AGENT_DENY_WRITE` | —（既定の規則のみ） | 読めるが書き込まないパターンのカンマ区切り（例: `Cargo.lock`）。既定の `.git/` に追加する |
| `AGENT_WEB_FETCH` | `false` | `web_fetch` ツールの有効化（§4 参照） |
| `AGENT_WEB_FETCH_ALLOW` | —（公開ホスト全般） | 許可ドメインのカンマ区切り（`docs.rs` は `*.docs.rs` も許可） |
| `AGENT_WEB_FETCH_ALLOW_PRIVATE` | `false` | プライベート帯拒否の解除（イントラネット用。通常は触らない） |
| `AGENT_WEB_FETCH_MAX_BYTES` | `1048576` | 取得サイズ上限 |
| `AGENT_WEB_FETCH_TIMEOUT_SECS` | `30` | 取得の上限時間 |
| `AGENT_SHELL` | `false` | `run_command` ツールの有効化（§4・§5 参照） |
| `AGENT_SHELL_SANDBOX` | `confined` | 必須とする封じ込め。`confined`（その OS で使える封じ込め。Linux 6.7+ は Landlock）\| `isolated`（プロキシ以外に到達できない層。**まだどの OS にも実装がなく、常に起動失敗**）\| `none`（封じ込めなし。使い捨てのコンテナ内でのみ）。満たせない環境では**起動失敗**（黙って弱くならない） |
| `AGENT_SHELL_NETWORK_ALLOW` | —（**通信不可**） | コマンドが到達できるドメインのカンマ区切り。空なら egress プロキシを起動せず全拒否 |
| `AGENT_SHELL_WRITABLE` | — | ワークスペース外で書き込みを許可するパスのカンマ区切り（外部のビルドキャッシュ用） |
| `AGENT_MCP_CONFIG` | —（**無効**） | MCP サーバの設定ファイル（JSON、**絶対パス**、ワークスペース外）。§5 参照。ワークスペース内なら起動失敗 |
| `AGENT_MCP_SANDBOX` | `confined` | MCP サーバに必須とする封じ込め。`confined` \| `isolated`（`AGENT_SHELL_SANDBOX` と同じ意味）。**`none` は指定できない**（`AGENT_SHELL_SANDBOX` とは独立） |
| `AGENT_PARALLEL_READ_TOOLS` | `true` | 読み取り系ツールの並列実行 |

### 通知（§3 参照）

| 変数 | 既定値 | 意味 |
|---|---|---|
| `AGENT_NOTIFY_WEBHOOK_URL` | —（**無効**） | 通知の送信先。秘密情報として扱い、表示は `scheme://host/…` のみ。不正な URL は起動失敗 |
| `AGENT_NOTIFY_FORMAT` | `json`（ホストが `ntfy.sh` なら `ntfy`） | `ntfy`（サーバのルートへ `{"topic","title","message","tags"}`）\| `slack`（incoming webhook へ `{"text"}`）\| `json`（`{"session_id","kind","title","message","tool"?,"elapsed_secs"?}`）。明示した値が推定より優先 |
| `AGENT_NOTIFY_EVENTS` | `approval,completed,failed` | 送る種別のカンマ区切り |
| `AGENT_NOTIFY_MIN_SECS` | `30` | これより短いターン完了は送らない。承認待ち・失敗には効かない |
| `AGENT_NOTIFY_EXCERPT_CHARS` | `100` | ターン完了の本文に載せる回答の文字数。`0` で載せない。**外部サーバに出る** |

### 生成とログ

| 変数 | 既定値 | 意味 |
|---|---|---|
| `AGENT_SYSTEM_PROMPT_FILE` | — | 既定のシステムプロンプトをこのファイルの内容で**差し替え**る。起動時に読まれ、欠落・空なら明確なエラーで落ちる（黙って既定に戻らない） |
| `AGENT_APPEND_SYSTEM_PROMPT` | — | 既定プロンプトの**末尾に追記**（プロジェクト指示ファイルよりさらに後ろ）。差し替えと併用時は差し替えた文面に追記 |
| `AGENT_GLOBAL_INSTRUCTIONS` | — | 操作者のグローバル指示ファイル（**絶対パス**。`~` は展開しない）。ワークスペースの指示ファイルより前に連結される（§4）。指定したのに読めなければ起動失敗。セッション開始時に 1 回読む |
| `AGENT_STREAM` | `true` | 応答の逐次表示（SSE）。サーバが `stream` / `stream_options` を拒否する場合、一括表示に戻したい場合、リクエスト全体の再試行を有効にしたい場合は `false` |
| `AGENT_MAX_TOKENS` | —（Anthropic は `16000`、OpenAI 互換は送らない） | 生成トークン上限。指定すれば全プロバイダにそのまま送る（OpenAI 互換ではフィールド名が `AGENT_OPENAI_MAX_TOKENS_FIELD` / `AGENT_PROVIDER_<別名>_MAX_TOKENS_FIELD` に従う）。未設定時、Anthropic は必須項目なので `16000`（拡張思考の思考トークンもここから消費される）、OpenAI 互換サーバには**送らずサーバに任せる**（文脈長 4K の小さなローカルモデルに大きな上限を送って詰まらせないため） |
| `AGENT_TEMPERATURE` | —（送らない） | サンプリング温度。未設定ならどのプロバイダにも送らない（履歴圧縮の要約リクエスト、`agent doctor` の疎通確認も同様）。**現行の Claude モデルは `temperature` を HTTP 400 で拒否する**（Opus 5.5 / Opus 5 / Sonnet 5 / 4.7・4.8 系は値に関係なく、Sonnet 5.5 / Haiku 5.5 は既定値以外を、API バージョン `2026-10-21` 以降は全モデルで）。指定すれば Anthropic にも送るが、`agent doctor` が警告を出す。`AGENT_THINKING=adaptive` のときは送らない |
| `AGENT_REQUEST_TIMEOUT_SECS` | `180` | HTTP リクエストの上限時間 |
| `AGENT_MAX_RETRIES` | `3` | 一時障害の再試行回数 |
| `AGENT_PROMPT_CACHE` | `5m` | Anthropic のプロンプトキャッシュ。`5m` \| `1h` \| `off`。ツール定義＋システムプロンプトの末尾と会話の末尾（自動キャッシュ）に印を付け、毎ターン同じ前置きを全額で読み直させない。プロンプトの間に 5 分以上空くことが多いなら `1h`（書き込み単価は 2 倍）、`cache_control` を拒否するゲートウェイ経由なら `off`。OpenAI 互換サーバ側のキャッシュは自動なので影響しない。効果は `-v` と `/usage` の `cache: N read, M written` で確認できる |
| `AGENT_THINKING` | `off` | Anthropic の拡張思考。`off`（`thinking` フィールドを**送らない**。`disabled` は送らないので、思考を止められないモデルではモデル既定のまま）\| `adaptive`（`"thinking": {"type": "adaptive"}`。思考するかと量をモデルが決める）。`adaptive` では `AGENT_TEMPERATURE` を送らない（思考と温度指定は併用できない）。思考は `AGENT_MAX_TOKENS` を消費するので、上げておくとよい。応答中の `thinking` / `redacted_thinking` ブロックは表示せず、同じ会話の後続リクエストにそのまま送り返す（署名検証・推論の継続・キャッシュのため）。`budget_tokens` は使わない。OpenAI 互換サーバには影響しない |
| `AGENT_EFFORT` | —（送らない） | Anthropic の `output_config.effort`。`low` \| `medium` \| `high` \| `xhigh` \| `max`。未設定ならフィールド自体を送らず、モデルの既定に従う |
| `RUST_LOG` | `warn` | tracing フィルタ（例: `info`、`agent_application=debug,warn`） |
| `RUST_BACKTRACE` | `0` | `1` でエラー時にバックトレース（テストの調査用） |
| `OLLAMA_PORT` | `11434` | compose の ollama をホストに公開するポート（ホスト側の compose 設定） |

`AGENT_THINKING` と `AGENT_EFFORT` は会話の途中で変えるとプロンプトキャッシュが無効になります。
どちらもプロセス単位の設定（起動時に読むだけ）なので、1 つのセッション内で変わることはありません。
`--resume` で再開するときに値を変えると、再開後の最初のリクエストはキャッシュを読めません。

---

今後の計画は [`management/issues/`](management/issues/) で管理しています。
