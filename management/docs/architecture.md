# アーキテクチャ

## 1. レイヤ構成

```
┌──────────────────────────────────────────────────────────────┐
│ agent-cli                    プレゼンテーション + 合成ルート  │
│   args.rs        clap 定義                                    │
│   render.rs      AgentEvent → 端末出力                        │
│   approval.rs    対話的な承認ゲート                           │
│   composition.rs 具象型とポートを結線する唯一の場所           │
│                  assemble() を run / chat / eval / テストが共有│
│   lib.rs / main.rs  lib の上に薄い bin（tests/ から呼べる）   │
└───────────────┬──────────────────────────┬───────────────────┘
                │                          │
┌───────────────▼──────────────┐ ┌─────────▼───────────────────┐
│ agent-application            │ │ agent-infrastructure        │
│  ユースケース                │ │  アダプタ                   │
│   agent/loop_runner.rs       │ │   llm/openai.rs             │
│   agent/dispatch.rs          │ │   llm/anthropic/            │
│   agent/prompt.rs            │ │   llm/routing.rs            │
│   agent/session.rs           │ │   llm/retry.rs              │
│   tools/file/*.rs            │ │   llm/http.rs（共有配管）   │
│   tools/registry.rs          │ │   llm/sse.rs（SSE 枠組）    │
│   eval/（検査・計測・集計）  │ │   fs/local.rs               │
│                              │ │   fs/scratch.rs（使い捨て） │
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
| `LlmProvider` | `OpenAiCompatibleProvider`, `AnthropicProvider`, `RoutingProvider`, `RetryingProvider`, `FailoverProvider` | チャット補完 |
| `LlmRouter` | `StaticRouter`, `ModelPrefixRouter`, `TaskKindRouter` | 委譲先の選択 |
| `Tool` | `ReadFileTool` ほか（application）, `TimeoutTool`・`RedactingTool`・`DependencyWatchTool`（装飾） | モデルに公開する能力。`preview` で書き込み予定を返す（§3.6.2） |
| `FileSystem` | `LocalFileSystem` | サンドボックス化された I/O |
| `FileSearcher` | `IgnoreAwareSearcher` | `.gitignore` 準拠の内容検索 |
| `ContextProvider` | `WorkspaceContextProvider` | 環境情報とプロジェクト指示の収集 |
| `PromptBuilder` | `DefaultPromptBuilder`, `FixedPromptBuilder`, `AppendingPromptBuilder` | システムプロンプトの組み立て方針（既定・差し替え・追記） |
| `ApprovalGate` | `CliApprovalGate` | human-in-the-loop。依存の変更は `authorize_dependency_change` で別に問う（§3.6.2） |
| `WebFetcher` | `GuardedWebFetcher` | 外向き HTTP（SSRF ガード・許可リスト・サイズ上限） |
| `CommandRunner` | `SandboxedCommandRunner` | 子プロセス実行（OS レベルの封じ込め・egress 許可リスト） |
| `ToolServer` | `McpServer` | 長寿命のツールサーバ（MCP over stdio）。サーバは封じ込めの中で動き、ツール一覧は起動時に固定（§3.2.4） |
| `EventSink` | `TerminalRenderer`, `NullEventSink`, `MetricsSink`（eval の計測） | 進捗の可観測性 |
| `ConversationLog` | `FileConversationLog`, `NullConversationLog` | 会話の永続的な記録（§3.2.3） |
| `SessionStore` | `FileSessionStore` | その記録の読み出しと再開（§3.2.3） |

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

### 3.2.2 履歴は削る前に畳む

コンテキスト予算を守る方法は 2 つあり、既定では両方を順に使います。

刈り込みだけの構成には、静かで気づきにくい失敗モードがあります。**最初に消えるのは
最も古いメッセージ、つまりユーザーが要件を述べた行です。** その結果は「動かない」ではなく
「決着した話を蒸し返す」「最初の制約に反する提案をする」であり、原因を辿るのが困難です。

圧縮はこれを、要約 1 通への置換に置き換えます。要約プロンプトが**節を名指しで要求する**のは、
自由記述だと読みやすい散文が出てくる代わりに、ユーザーが付けた制約が静かに落ちるからです。
モデルには「自分の推論はこの記録の中で最も安く、ユーザーの一行の訂正が最も高い」という
判断基準がありません。節を並べることが、欠落を可視化する唯一の手段です。

置き場所は層で分かれています。**どこで切ってよいか**は `Conversation` の不変条件なので
domain（`compaction_split` / `replace_prefix`）に、**要約に何を書かせるか**はユースケースなので
application（`agent/compaction.rs`）にあります。

**何が新しくて何が古いかは、順序と大きさで決まります。時計は使いません。**
各メッセージは `Conversation` が付ける通番（`Message::seq`）を持ち、保持方針が読むのは
`distance_from_newest`、つまり「それ以降に何通の会話があったか」です。壁時計を使わないのは、
3 時間放置しただけのセッションは何も先に進んでいないのに時計はそれを「古い」と言うからで、
副産物としてドメインにクロックのポートが不要になり、方針のテストが sleep なしで書けます。

そのまま残す範囲は `compaction_split_within` が**通数と容量の両方**で頭打ちにします。
通数だけでは尺度になりません — ツール結果 12 通は履歴予算そのものより大きく、
守るはずだった直近のやり取りが書いたばかりの要約を押し出します。畳む側も境目で
ばっさり切らず、1 ブロックの取り分が後ろに下がるほど半減します（下限あり）。
ただし**ユーザーの発言は距離に関係なく無傷**で、これが前回の要約（user ロールで入る）を
守っています。ここを外すと、圧縮のたびに前回の圧縮を打ち消すことになります。

### 3.2.3 短くする前に書き出す

圧縮も刈り込みも元に戻せません。そこで `AgentLoop` は履歴に追記するたび、
**`fit_history` が走る前に** `ConversationLog` ポートへ渡します。順序がすべてで、
そこまでに書かれなかったものが記録から欠けるものです。

`EventSink` と役割が違う点に注意してください。イベントは端末への実況で、
`AssistantMessage` は散文だけを運びツール結果は 1 行要約に縮められています。
表示にはそれが正しく、記録にはそれでは足りません。だから別のポートです。

失敗は警告して捨てます。ログが書けないことでセッションが止まるのは、
1 行欠けるより悪い結果です。

**再開はこのログの再生です。**保存用のスナップショットを別に持ちません。同じことを
言うはずのファイルが 2 つあると、いずれ食い違います（クラッシュが 2 つの書き込みの間に
入るだけで足ります）。そうなったとき、どちらが正しいかを言えるものは何もありません。

書き手（`ConversationLog`）と読み手（`SessionStore`）でポートを分けているのは、
ループに必要なのは追記だけで、セッションを削除する権限を渡す理由がないからです。

再生は**壊れた行で止めます。読み飛ばしません。**読み飛ばしの方が寛容に見えて実際は悪く、
読めなかった行に `tool_call` があると対応する `tool_result` が孤児になり、
次のリクエストがまるごと拒否されます。止めれば前半が残り、正しい履歴の前半は正しい履歴です。
最後が未応答の `tool_call` で終わる場合は `drop_trailing_unanswered_calls` が落とします。

**保持期間は起動時に適用します。**ログは追記しかしないので、何かが「消す側」に
ならなければディレクトリは一方向にしか動きません。`FileSessionStore::prune` は
ディレクトリを 1 回読んで各ファイルを `stat` するだけで、セッションを開きません。

これは `SessionStore` ポートには載せていません。**転写がディスクに何日残るかは
ストレージの性質であって、会話についての規則ではない**からです。載せると、ドメインが
他では要らない保持ポリシーと「日」という単位を持つことになります。

同じ理由で一覧も**ファイルを開きません**。`SessionSummary` は id・冒頭の依頼・バイト数だけで、
どれもメタデータと先頭数 KB で答えられます。通数は全行が要るうえに 3 つの中で最も情報量が
少ない（どれが自分のセッションかと、どれを消すべきかが分かればよい）ので落としました。

失敗時に何も壊れないのは順序のおかげです。要約が手に入るまで履歴を変更しないので、
「半分圧縮された履歴」という状態が存在しません。プロバイダが落ちれば単に従来の削除に戻ります。

要約リクエストが元のメッセージ列ではなく**平坦化したテキスト 1 通**なのは意図的です。
ツール定義を宣言しないリクエストに `tool_call` ブロックを載せた場合の挙動はプロバイダ間で
一致しておらず、これは「セッションが既に苦しいときに必ず通らなければならない」唯一の
リクエストだからです。平坦化はツール出力の切り詰めも可能にします（ユーザー発言だけは
切りません。それが最も失ってはいけないものだからです）。

### 3.2.4 ツールサーバ（MCP）は 3 層に分ける

外部の MCP サーバのツールをモデルに渡す経路は、責務ごとに層を分けています。

- **domain の `ToolServer`** — 中立名のポート（`server_name` / `sandbox()` / 起動時に固定した `tools()` / `call`）。
  MCP という語も JSON-RPC の型も出さない。`sandbox()` は `CommandRunner` と同じく実際に効いている封じ込めを返す
- **infrastructure の `mcp/`** — JSON-RPC を serde_json の上に自前で書く（SDK を足さない。封じ込めを担う部品の
  サプライチェーンを増やさないため）。応答は id で振り分け、future の drop で `notifications/cancelled` を送る。
  stdin に書くのは専用の writer タスクだけで、呼び出し側は行全体をキュー（上限 64 件）に渡す。`write_all` は
  キャンセル安全でないため、呼び出し側の future が書く形だと drop で半端な行が残り、次のメッセージがそこへ連結される。
  サーバが stdin を読まずに要求を送り続けて上限を超えたら停止扱いにする。
  1 メッセージ 4 MiB、stderr は読み捨て続け、drop で stdin を閉じてからプロセスグループを kill し、
  グループから抜けた子孫も止める（`exec/reaper.rs`）。
  プロセスは `run_command` と同じ `exec::confined::confine_command` で封じ込めるが、書き込み可能なのは
  サーバ専用ディレクトリだけ（ワークスペースは設定で明示したときだけ）。サーバは呼び出しの合間にも動けるので、
  承認ゲートも依存ファイルの検知も通らない書き込みを既定では許さない
- **application の `tools/server.rs`** — 名前空間（`mcp__<server>__<tool>`）、文字種の置換と 64 文字の切り詰め、
  衝突の検出（registry は黙って上書きするので起動時に失敗させる）、説明とスキーマの無害化と上限、
  出所の前置き、安全区分は一律 `Destructive`（サーバの自己申告を信用しない）。どれも純粋関数でテストする

設定ファイル（`AGENT_MCP_CONFIG`）は操作者のものとして扱い、ワークスペース内にあれば起動失敗、
起動するプログラムがワークスペース内でも起動失敗にしています。設定はプログラムを起動する権限そのものだからです。
読み取りは閉じられないので、設定ファイルに直接書いた値は全サーバと `run_command` から読めます。秘密は
`env_from`（エージェント自身の環境からの名前指定）で渡し、その値は伏せ字の対象かつ他の子プロセスの環境から値で除去します。
initialize の失敗やサンドボックスの欠如はエージェント全体の起動失敗です（設定したツールが黙って欠けない）。
合成ルートは `redact_all` の前に `TimeoutTool` で包んで登録するので、最終的な形は
`Redacting(DependencyWatch(Timeout(ServerTool)))` になります。

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

ループ側は `dispatcher.dispatch(&calls, &interrupt).await` の 1 行になり、
承認・並列化・出力切り詰めの詳細を知りません。

### 3.6.1 協調的中断（Ctrl-C）

ターンを止めるのに**ループの future を drop しません**。drop すると、アシスタントの `tool_call` を
会話とログに積んだ直後のような「結果のない呼び出し」で履歴が終わり、次のリクエストが拒否されます（不変条件 §4）。

代わりに `application/src/agent/interrupt.rs` の `Interrupt`（std のみ・Clone で共有・一度発火したら戻らない・
複数 waker 対応）をターンごとに `run_until(session, input, &interrupt)` へ渡し、ターンの側が止まります。
ポートにしないのは差し替えるものがないからで、tokio に依存しないので application に置けます（§1）。

- 確認点は **反復の先頭・圧縮の要約リクエスト・モデル要求（ストリーム含む）・ディスパッチャ内**（承認待ち・並列・直列）だけ
- ディスパッチャは中断されても**入力と同じ数の結果を要求順で**返す。完了済みは実結果、未開始は「実行されていない」、
  実行中は「一部反映されている可能性がある。状態を確かめてから」（どちらも `is_error: true`）
- 順序は push → record → 中断なら return。呼び出しを会話から**削除して直さない**（追記専用のログと食い違う）
- 中断は `Ok(AgentOutcome { reason: FinishReason::Interrupted })` で返す。`Err` はプロバイダが壊れたときだけ（§3）
- ストリーミング途中の断片は履歴に残さず、`[interrupted]` のような文言も足さない

cli 側（`cli/src/ctrl_c.rs`）は「プロンプト待ち → 終了(0) / ターン中 → 発火 / 後始末中にもう一度 → 放棄して終了(130)」
の状態機械で、時間窓ではなく状態で判定します。SIGINT のハンドラは `chat` と `run` の中だけで登録します。
stdin はランタイム外の専用スレッド + チャネルで読むので、中断で放棄した承認プロンプトが次の 1 行を飲み込みません。
`run_command` の子は drop guard（`infrastructure/src/exec/capture.rs`）が reap 前にプロセスグループごと止め、
`setsid` でグループから抜けた子孫も subreaper 経由で探して止めます（`infrastructure/src/exec/reaper.rs`）。

### 3.6.2 依存ファイルの変更を別に承認する

マニフェストとロックファイルへの変更は、次のビルドで実行される第三者のコードを決めます。
`auto` や「以降すべて許可」の後でも素通りさせないため、通常の承認とは別の問いにしています。

- **判定は domain**（`model/dependency.rs` の `ManifestKind::classify`）。ファイル名を ASCII 小文字で比較し、
  ディレクトリは問わない。大文字小文字を無視しないファイルシステムでも同じファイルを指しうるため
- **承認はディスパッチャの Phase 2、各 invoke の直前**。`Tool::preview` で書き込み予定（パス・現在の内容・書き込み後の内容）
  を受け取り、依存ファイルの中身が変わるなら `ApprovalGate::authorize_dependency_change` を呼ぶ。
  Phase 1 ではなくここに置くのは、同じターンの先行する書き込みを反映した差分を見せるため。
  `TimeoutTool` の内側（ツールの execute）に置かないのは、人の応答待ちがツールのタイムアウトで打ち切られるため。
  中断は通常の承認と同じく「実行していない」になる
- **`preview` に既定実装はない**。装飾子が転送を忘れると「書き込みなし」と答えて承認を素通りさせるので、
  忘れたらコンパイルエラーになるようにしている。失敗した `preview` はそのまま invoke に進む（同じ理由で失敗し、何も変えない）
- **`authorize_dependency_change` の既定は拒否**。この問いを知らないゲートが「全部通す」ゲートにならないため。
  CLI の実装は対話ならポリシーも approve-all も見ずに必ず聞き、非対話は拒否。`AGENT_ALLOW_DEPENDENCY_CHANGES` は
  `ApprovalPolicy` とは独立した明示的な逃げ道
- **聞くかどうかは「分類されたパス + 内容の不一致」だけで決まる**。差分の説明は application の `supply_chain/`
  （行ベースと serde_json。TOML / YAML のパーサは足さない）が作るが、読めなくても変わった行を見せて必ず聞く
- **`run_command` は事後検知**。infrastructure の `DependencyWatchTool` が `Destructive` のツールを
  `TimeoutTool` の外側から包み、前後で依存ファイルのハッシュを比べる。変化は結果の先頭への注記と
  `AgentEvent::DependenciesChanged` で知らせる。止めるものではない
- 既知の穴: シンボリックリンク／ハードリンクによる別名。脆弱性 DB の照会は未実装

不変条件（`invariants.md`）には入れていません。保証の強さが経路で違い（ファイルツールは事前、コマンドは事後）、
「常に成り立つ契約」として書けないためです。

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

stderr と承認プロンプトに出す文字列（ツール引数・結果の要約・拒否理由・計画・ファイル名）は、
**出口で 1 回だけ** `agent_domain::text::printable` を通します。制御文字（C0 / C1、タブを含む）・
双方向制御文字・不可視文字を `?` に置き換えるもので、`\r` と行消去のエスケープで承認プロンプトを
書き換え、実際とは違うコマンドを見せて承認させる攻撃を防ぎます。上流（`ApprovalRequest::summary`
やイベント）は生のまま運び、描画する側が必ず通す、という分担です。`TerminalRenderer` では
色付けの `paint` が通すので、描画のたびに思い出す必要がありません。モデルの散文（stdout）は対象外です。

### 3.9 評価（`agent eval`）をループの外に置く

プロンプトやループの変更を数字で比べるための `agent eval` は、ループに計測用の口を足さずに作っています。

| 部品 | 場所 | 理由 |
|---|---|---|
| タスク定義・検査 `verify`・集計・指紋（FNV-1a） | `application/src/eval/` | 何を成功とみなし、どう数えるかはユースケース。I/O は `FileSystem` / `CommandRunner` ポート越し |
| 計測 `MetricsSink` | 同上 | ループが既に出している `AgentEvent` を数えるだけの `EventSink`。第 2 の報告経路を作らない |
| フィクスチャの複製 `ScratchDir` | `infrastructure/src/fs/scratch.rs` | std のみ。シンボリックリンクは拒否、drop で削除（`--keep` で残す） |
| 試行ごとの組み立て・時間計測・出力 | `cli/src/commands/eval.rs` | エージェントは `composition::assemble` で組む。測っているものが `agent run` の配線そのものであるため |

決めたこと:

- **設定の誤りは LLM を呼ぶ前に落とす。** タスクファイルの検証、フィクスチャと `prompt_file` の存在、
  空のスクラッチ上での `assemble`、`command` 検査用サンドボックスの起動をすべて先に済ませる
- **`command` 検査は常に封じ込める。** 検査はモデルが書いたコード（`build.rs`、テスト）を実行するので、
  `AGENT_SHELL_SANDBOX=none` は検査には適用しない（`confined` に引き上げる）。同じ理由で、
  eval では封じ込めのない `run_command` をモデルに渡さない
- **stdout はレポート専用**（§3.8 と同じ分離）。進捗は stderr、会話ログは `--out` 配下
- **指紋のプロンプトハッシュは固定の文脈で描画したもの。** 実際に送ったプロンプトはスクラッチのパスと
  日付を含み、実行ごとに変わってしまうため
- **圧縮の要約リクエストの usage を `session.usage` に積む。** 以前は捨てていたため、
  圧縮が起きる長いセッションほどトークンが少なく見えていた（`CompactionReport.usage`）

記録の再生（replay）は作っていません。必要になったら `fit_history` を部品として切り出し、
記録を流すシミュレーションとして別に作る方針です。

## 4. コンテキストエンジニアリング

システムプロンプトは 1 セッションにつき 1 回だけ組み立てられ（`Session` が保持）、以下を含みます。

1. 環境情報（ルート、OS、日付、git かどうか）
2. ワークスペース概要（深さ 2 / 最大 60 件）
3. ツール一覧（説明の 1 行目のみ。全文はツール定義側にある）
4. 作業ルール 9 項目
5. プロジェクト指示ファイル（最大 8 KB で切り詰め）

ツール一覧の順序は `BTreeMap` で安定させています。プロンプトが実行ごとに揺れると
プロバイダ側のプロンプトキャッシュが効かなくなるためです。同じ理由で、`chat` の 2 ターン目以降も
最初に組み立てたプロンプトを使い回します。ワークスペースを読み直すと、エージェント自身が書いた
ファイルでディレクトリ概要が変わり、ツール定義＋システムプロンプトから後ろのキャッシュがすべて外れます。

Anthropic アダプタはこの前置きにキャッシュの印を付けます（システムプロンプトの最後のブロックと、
会話の末尾を追う最上位の `cache_control`。TTL は `AGENT_PROMPT_CACHE`）。要約リクエストは
一度きりの書き起こしを書き込む価値がないため、会話側の印を付けません。キャッシュの読み書き量は
`TokenUsage` の `cache_read_tokens` / `cache_write_tokens` に入り、どちらも `input_tokens` の内訳です。

履歴は毎イテレーション前に `Conversation::trim_to_budget` で刈り込まれ、
末尾 `keep_recent_messages` 件は必ず残ります。刈り込むときは予算ちょうどではなく
予算の 3/4（低水位）まで一度に落とします。予算ちょうどまでだと、予算に張り付いた会話は
毎イテレーション先頭が 1〜2 通ずつ消え、キャッシュされた messages の前置きが毎回書き直しに
なるためです（読み 0.1x のはずが書き込み 1.25x）。水位はループ側の方針で、domain の契約は
「予算以下」のままです。圧縮が成功した直後は低水位を使いません（残したばかりの原文を消さないため）。

### 作業計画の置き場所

`todo_write` の計画は**会話が真実**です。現在の計画は「成功した最後の `todo_write` 呼び出しの引数」
であり、ツール自体は状態を持ちません（`application/src/tools/plan/`）。規則（作業中は 1 つまで、
20 項目・各 1 行 200 バイトまで、制御文字・双方向制御文字・不可視文字を含まない）は
domain の値オブジェクト `Plan` が持ち、違反は直さず拒否します。

結果と呼び出しの対応づけは、結果の直前の assistant メッセージの中で行います。呼び出し id は
ターン内でしか一意ではありません（id を返さないサーバには OpenAI 互換アダプタが毎ターン
`call_0` から振ります）。履歴全体を id で引くと、拒否された計画の結果を前のターンの引数と
取り違えます。

`Session.plan` はその射影のキャッシュです。履歴が短くなると計画を書いた呼び出し自体が消えるため、
ループはツール結果を積むたびに最新ターンだけを射影して更新し、変わったときだけ
`AgentEvent::PlanUpdated` を出します。再開時は `Session::resumed` が記録全体から射影し直します。

計画をモデルに見せ直すのは `fit_history` が履歴を短くした直後だけで、未完了の項目があり、
かつ計画がもう履歴から見えないときに user ロールのメモを 1 回だけ末尾に追記・記録します。
システムプロンプト・`PromptBuilder`・`ContextProvider` には入れません。前置きが毎回変わり
キャッシュが外れるうえ、`FixedPromptBuilder` で差し替えると消えるためです。メモの分は
刈り込みの予算から先に差し引くので、メモ自体が次の刈り込みを誘発することはありません。

## 5. テスト戦略

| 層 | 手法 | 例 |
|---|---|---|
| domain | 純粋な単体テスト | パス脱出の拒否、履歴刈り込みの不変条件、文字境界 |
| application | 全ポートをフェイク | ループの分岐、並列実行の順序保証、承認拒否 |
| infrastructure | `MapEnv` を注入 | 別名の大文字化、空文字の扱い、キーのフォールバック |
| infrastructure | ペイロード写像の単体テスト | `finish_reason:"stop"` + `tool_calls` の解釈 |
| infrastructure | 生 TCP のモックサーバ | ステータス写像、実際に送出される JSON |
| 全体 | E2E（`crates/cli/tests/end_to_end.rs`） | HTTP → ループ → 実ファイル書き込み |
| 全体（実バイナリ） | 子プロセス E2E（`crates/cli/tests/notify_e2e.rs`） | 合成ルート → `run` / `chat` → 終了処理。Webhook 通知の到達・中身・終了コード |
| 全体（実バイナリ） | 子プロセス E2E（`crates/cli/tests/plan_e2e.rs`） | `todo_write` の登録・承認なしでの実行・stderr への表示・`--resume` での復元・`/reset` |
| 全体（実バイナリ） | 子プロセス E2E（`crates/cli/tests/dependency_gate_e2e.rs`） | 非対話の `auto` で `Cargo.toml` の編集が拒否されファイルが変わらない、`AGENT_ALLOW_DEPENDENCY_CHANGES` で通る |
| 全体（実バイナリ） | 子プロセス E2E（`crates/cli/tests/checkpoint_e2e.rs`） | `/undo` でファイルが戻る・手で編集されたファイルは上書きしない・`/rewind` 後の `--resume` で巻き戻した後の会話から始まる |
| 全体（実バイナリ） | 子プロセス E2E（`crates/cli/tests/interrupt_e2e.rs`） | SIGINT で `run` が 130、`chat` はターンだけ止まりプロンプトに戻る、プロンプト待ちの Ctrl-C は 0 |
| infrastructure（実プロセス） | `/bin/sh` 製の MCP サーバ（`crates/infrastructure/tests/mcp_sandbox.rs`） | 既定でワークスペースに書けない・自分のディレクトリには書ける・許可外へ接続できない・drop で孫まで止まる・initialize 失敗で起動失敗 |
| 全体（実バイナリ） | 子プロセス E2E（`crates/cli/tests/mcp_e2e.rs`） | MCP ツールの登録と `destructive` 分類、伏せ字・タイムアウト・依存の検知・承認の各装飾、終了時に孫が残らない、設定の位置による起動失敗 |

モックサーバは `crates/test-support` に置いて infrastructure と cli の
両テストスイートで共有しています（HTTP フレームワークを足さずに済ませるため、
実装は生 TCP の約 60 行です）。

外部依存はゼロなので、`make test` はネットワークもモデルも要りません（計 925 本）。
