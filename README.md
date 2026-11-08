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
make run ARGS="-v run 'hi'"    # 任意の引数で CLI を実行
```

`chat` 内のコマンド: `/reset` `/usage` `/tools` `/help` `/exit`

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
| `web_fetch` | network | URL の内容をテキスト取得（**既定で無効**。`AGENT_WEB_FETCH=true` で有効化） |
| `run_command` | destructive | シェルコマンドをサンドボックス内で実行（**既定で無効**。`AGENT_SHELL=true` で有効化） |

`edit_file` を曖昧一致にしないのは意図的です。モデルが `read_file` で見た文字列と
一致しない限り編集は成立せず、意図しない箇所への適用が構造的に起こりません。

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
- **プロジェクト指示ファイル** — `AGENTS.md` → `CLAUDE.md` → `.agent/instructions.md` →
  `.github/copilot-instructions.md` の順で最初に見つかったもの
- ツール一覧と作業ルール

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
> 既定ではワークスペース内に置かれるので、**モデル自身の `read_file` からも読めます**。
> 気になる場合は `AGENT_SESSION_DIR` にワークスペース外の絶対パスを指定するか、
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
- `*_API_KEY` / `*_TOKEN` / `*_SECRET` 等の環境変数は子プロセスに渡しません
- 外向き TCP は egress プロキシのポート以外すべて拒否。プロキシは
  ドメイン許可リスト（`AGENT_SHELL_NETWORK_ALLOW`）で `CONNECT` と絶対形式リクエストを
  検査します。許可リストが空なら**プロキシを起動せず、通信は一切できません**
- タイムアウト時はプロセスグループごと SIGKILL（バックグラウンドの孫プロセスを残さない）

**この層の限界も明記します。** Landlock のルールはカーネルが *union* する純粋な
許可リストで、拒否も「より具体的なルールが勝つ」規則もありません。したがって
書き込み可能なワークスペースの中の `.git` だけを読み取り専用にすることは**表現できません**
（`landlock_cannot_protect_git_inside_the_writable_workspace` テストがこの制限自体を固定しています）。
また Landlock のネットワークルールは**ポート単位**で宛先アドレスを持たないため、
「プロキシのポート」に居る別サービスには到達し得ます。どちらも network namespace 層
（`SandboxKind::NetnsProxied`）で塞ぐべきもので、別 issue として管理しています。

`AGENT_SHELL_SANDBOX` が満たせない環境では**起動に失敗します**。黙って弱い設定に
フォールバックしないのは、静かに劣化するサンドボックスが最も危険だからです。

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
  401 / 400 などは設定の誤りなので切り替えずにそのまま失敗します（切り替えると誤りが
  隠れるため）。切り替え先には切り替え先自身の設定モデルを要求します
- **ストリームは途中で切り替えません** — 出力が始まった後の失敗はそのまま失敗として
  返ります。切り替えると、途中まで表示した回答に別の回答が継ぎ足されるためです
- **クールダウン** — 一度切り替わった委譲先は `AGENT_FAILOVER_COOLDOWN_SECS`（既定 60 秒）
  の間スキップし、直接切り替え先へ送ります。経過後は再び第一候補から試すので、
  復旧すれば自動で戻ります。クールダウン中でも切り替え先が全滅したら第一候補を最後に試します
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

テストは 402 本、外部ネットワークもモデルも不要です。

- ドメイン／ユースケース … フェイクのポートのみ
- 設定 … `EnvSource` 経由でインメモリ環境を注入（プロセス環境を汚さない）
- LLM クライアント … 生 TCP のモックサーバで実 HTTP を検証
- E2E … HTTP → provider → routing → retry → loop → tools → 実ファイルシステム
  （`crates/cli/tests/end_to_end.rs`）
- 実バイナリ … `agent` を子プロセスとして起動し、モック LLM とモック Webhook だけを相手に
  合成ルートと終了処理まで通す（`crates/cli/tests/notify_e2e.rs`）

---

## 8. 設定リファレンス

以下が全変数です。必須は `AGENT_PROVIDER` / `AGENT_API_KEY` / `AGENT_MODEL` の 3 つだけで
（Ollama ならキーも不要）、`.env.example` にはこの 3 つしか載せていません。
それ以外はすべて任意で、表の既定値で動きます。

どの変数も `.env` に書いても、通常の環境変数として渡しても同じです（`.env` は compose 用の便宜です）。
値が空の行（`AGENT_API_KEY=`）は未設定として扱います。

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
| `AGENT_MAX_ITERATIONS` | `25` | 1 ターンあたりのモデル往復上限 |
| `AGENT_MAX_TOOL_OUTPUT_BYTES` | `32768` | 履歴に入るツール出力の上限 |
| `AGENT_MAX_HISTORY_BYTES` | `262144` | 履歴の予算。超過分は圧縮、それでも溢れれば古い順に破棄 |
| `AGENT_COMPACT` | `true` | 溢れた履歴を要約に畳む（§4 参照）。`false` で従来どおり削除のみ |
| `AGENT_COMPACT_KEEP_RECENT` | `12` | 圧縮後もそのまま残す直近メッセージ数の上限 |
| `AGENT_COMPACT_KEEP_RECENT_BYTES` | `65536` | 同じくバイト数の上限。通常こちらが先に効く（§4 参照） |
| `AGENT_SESSION_LOG` | `true` | 会話を追記ログに残す（§4 参照）。会話全文がディスクに残る |
| `AGENT_SESSION_DIR` | `.agent/sessions` | ログの置き場所。相対パスは `AGENT_WORKSPACE` 起点 |
| `AGENT_SESSION_KEEP` | `50` | 残すセッション数の上限。`0` で無制限 |
| `AGENT_SESSION_MAX_AGE_DAYS` | `30` | 残す日数の上限。`0` で無期限 |
| `AGENT_TOOL_TIMEOUT_SECS` | `60` | ツール 1 回あたりの上限時間 |
| `AGENT_MAX_FILE_BYTES` | `2097152` | `read_file` が読む最大サイズ |
| `AGENT_WEB_FETCH` | `false` | `web_fetch` ツールの有効化（§4 参照） |
| `AGENT_WEB_FETCH_ALLOW` | —（公開ホスト全般） | 許可ドメインのカンマ区切り（`docs.rs` は `*.docs.rs` も許可） |
| `AGENT_WEB_FETCH_ALLOW_PRIVATE` | `false` | プライベート帯拒否の解除（イントラネット用。通常は触らない） |
| `AGENT_WEB_FETCH_MAX_BYTES` | `1048576` | 取得サイズ上限 |
| `AGENT_WEB_FETCH_TIMEOUT_SECS` | `30` | 取得の上限時間 |
| `AGENT_SHELL` | `false` | `run_command` ツールの有効化（§4・§5 参照） |
| `AGENT_SHELL_SANDBOX` | `confined` | 必須とする封じ込め。`confined`（その OS で使える封じ込め。Linux 6.7+ は Landlock）\| `isolated`（プロキシ以外に到達できない層。**まだどの OS にも実装がなく、常に起動失敗**）\| `none`（封じ込めなし。使い捨てのコンテナ内でのみ）。満たせない環境では**起動失敗**（黙って弱くならない） |
| `AGENT_SHELL_NETWORK_ALLOW` | —（**通信不可**） | コマンドが到達できるドメインのカンマ区切り。空なら egress プロキシを起動せず全拒否 |
| `AGENT_SHELL_WRITABLE` | — | ワークスペース外で書き込みを許可するパスのカンマ区切り（外部のビルドキャッシュ用） |
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
| `AGENT_STREAM` | `true` | 応答の逐次表示（SSE）。サーバが `stream` / `stream_options` を拒否する場合、一括表示に戻したい場合、リクエスト全体の再試行を有効にしたい場合は `false` |
| `AGENT_MAX_TOKENS` | `4096` | 生成トークン上限 |
| `AGENT_TEMPERATURE` | `0.2` | |
| `AGENT_REQUEST_TIMEOUT_SECS` | `180` | HTTP リクエストの上限時間 |
| `AGENT_MAX_RETRIES` | `3` | 一時障害の再試行回数 |
| `RUST_LOG` | `warn` | tracing フィルタ（例: `info`、`agent_application=debug,warn`） |
| `RUST_BACKTRACE` | `0` | `1` でエラー時にバックトレース（テストの調査用） |
| `OLLAMA_PORT` | `11434` | compose の ollama をホストに公開するポート（ホスト側の compose 設定） |

---

今後の計画は [`management/issues/`](management/issues/) で管理しています。
