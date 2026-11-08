# Claude Code 相当機能の設計（2026-11-08 確定）

domain-specialist の設計確認（HEAD `a1252dc` 時点）と、ユーザー・root の決定をまとめたもの。
実装担当はこの文書を**拘束力のある設計**として扱う。各節の「禁止」は必ず守る。

共通の前提:
- 承認系の機能（プランモード・権限ルール・前フック）は **`ApprovalGate` の装飾子**として積む。
  積み順（外 → 内）: `PlanModeGate(HookGate(RuleGate(CliApprovalGate)))`。
  装飾子は `authorize_dependency_change` を**必ず内側へ転送**する（既定実装は Deny）。
- 新しい設定は既定値を持たせ、能力を広げるものは `crates/cli/src/dotenv.rs` の `DENIED` に追加し、
  README §8 に書く。`.env.example` は必須 3 項目のみ。
- system プロンプトはセッション単位で固定（キャッシュ）。モードや状態の変化は**ユーザー発言の先頭に
  application の定型文を入れる**形で伝える（計画メモと同じ方式）。ツール一覧は変えない。
- 端末に出すモデル・外部由来の文字列は `domain::text::printable` を通す。

## 1. プランモード
- `PlanModeGate`（application、`Arc<AtomicBool>`）。判定は `safety.is_read_only()`（ツール名で見ない）。
  root 決定: Network（web_fetch / web_search）は**許可**（通常の承認は通る）。それ以外の非 read-only は拒否。
- chat に `/plan-mode on|off`。切り替え直後のユーザー発言の先頭に定型文。
- v1: ユーザーのコマンドでのみ切り替え（`exit_plan_mode` ツールは作らない）。`run --plan` は作らない。
- 禁止: ツール一覧の変更、system の再構築、ツール名の許可リスト。

## 2. 細かい権限ルール
- domain: `PermissionRule { tool: ToolName, subject: Option<SubjectPattern>, effect: Allow|Ask|Deny }`。
  パス照合は既存の `PathPattern`（`domain/src/model/access.rs`）を流用。新しい glob 照合器を作らない。
- 適用順は **deny > ask > allow**（最も厳しいもの）。
- 照合対象: ファイル系は `PlannedWrite.target`（実体）とモデル指定パスの**両方**。web_fetch の deny は
  `GuardedWebFetcher` のホスト方針にも入れる（リダイレクト対策）。run_command は **deny / ask のみ**
  （「安全境界ではなく誤操作の柵」と明記）。
- `RuleGate` を `CliApprovalGate` の外側に。root 決定: `ApprovalRequest` に `must_ask: bool` を domain に
  足す（ask を Auto ポリシーや「a」より優先させるため）。依存承認には一切関与しない。
- 設定: 運用者のファイルのみ（実環境変数で絶対パス、ワークスペース外。MCP の `refuse_inside` を流用）。
  `AGENT_PERMISSIONS*` は DENIED。ワークスペース内のルールファイルは v1 では読まない。
- 禁止: run_command の allow、生文字列照合、allow を依存承認やプランモードより優先させること。

## 3. チェックポイントと取り消し
- ユーザー決定: **`/undo` はファイルだけ**戻して会話は残す（次のユーザー発言の先頭に「何を戻したか」の
  定型文）。**`/rewind` はファイルと会話の両方**をそのターンの前まで戻す。
- `CheckpointingTool`（application の Tool 装飾子）: `execute` 直前に `inner.preview(args)` の
  `PlannedWrite { target, before, after }` を取り消し 1 件として `CheckpointJournal`（具象、ポートにしない）に積む。
  ターンの区切りは `run_until` がユーザー発言を push したときの seq。
- 復元規則（domain の純粋関数）: 今 == after → before に戻す / 今 == before → 何もしない /
  それ以外 → **衝突として戻さず報告**。`Before::Absent` は削除で戻す → `FileSystem::remove_file` を
  **既定実装なし**で追加（`guard()` と書き込み側 AccessPolicy を通す。フェイク 3 つも更新）。
  `Before::Unreadable` は戻せないと報告。
- 会話: `Conversation::rewind_to(seq)`（ユーザー発言の直前で切る = 対を壊さない）。root 決定:
  `ConversationLog` に**巻き戻しの印を追記するメソッド**を既定実装なしで追加し、`SessionStore::load` の再生で
  `seq >= S` を落とす。`SCHEMA_VERSION` を上げる。過去の行は書き換えない。
- v1: write_file / edit_file のみ、メモリ上（プロセス終了で消える、`--resume` 不可）。そのターンに
  run_command / MCP / 背景実行があれば「戻していない」と一覧表示。
- 禁止: ホストから `git` を呼ぶこと（`.git/config` 経由でサンドボックス外のコード実行になる）、
  衝突ファイルの上書き、ログの過去行の改訂。

## 4. `/model` `/compact` `/context`
- `/model`: `Session.model_override: Option<ModelId>` を `build_request` が優先。ターン間のみ。切り替え時に
  `Conversation::drop_opaque()`（domain）で thinking ブロックを捨てる。root 決定: 再開後に引き継がない（v1）。
- `/compact`: `AgentLoop::compact_now(&self, &mut Session, &Interrupt)`。計画メモの復元も同じ手順。
  root 決定: 手動圧縮は `AGENT_COMPACT=false` でも実行する。
- `/context`: application の `ContextReport`（system / tools / 履歴 / 計画のバイト数と予算、**直前 1 回の**
  `TokenUsage` = `Session.last_usage` を追加）。system 未構築なら「まだ組み立てていない」。
- 禁止: `/model` で `AgentLoop` を作り直すこと、system の再構築。

## 5. ヘッドレスの構造化出力
- `agent run --output-format text|json|stream-json`。cli に `JsonLinesSink`。**`AgentEvent` に Serialize を
  付けない** — cli 側の DTO、全行に `"v":1`。
- `json`: 最後に 1 オブジェクト（final_text, reason, usage, session_id, iterations）。
  `stream-json`: イベント DTO を 1 行ずつ。stdout は JSON のみ（散文を出さない）。
- 出力チャネルの規約（`.claude/rules/architecture.md`）を「json モードでは stdout が JSON」と更新。

## 6. フック
- 前フック: `HookGate`（ApprovalGate 装飾子）。人に尋ねる**前**に走らせ、0 以外（失敗・タイムアウト含む）は
  `Deny { reason: stderr }`。承認はできない（拒否だけ）。
- 後フック: `HookedTool`（infrastructure の Tool 装飾子、観測のみで結果を変えない）。
- 停止時フック: `cli/src/commands/turn.rs` の通知の隣でタイムアウト付き await。`EventSink` から起動しない。
- 実行: infrastructure の `HookRunner`。`exec/` の封じ込めと capture を流用し stdin を渡せる共通部品にする
  （`CommandRequest` は広げない）。root 決定: **サンドボックス内で実行するのが既定**。要求した封じ込めが
  得られなければ起動失敗。
- stdin: `{"v":1,"event":"pre_tool_use","session_id","tool","safety","call_id","arguments"}`、
  後フックは `is_error` と redact 済み・切り詰め済みの `output` を追加。
- 設定: 実環境変数の絶対パスのみ、ワークスペース内は拒否（`refuse_inside` 流用）、`AGENT_HOOKS*` は DENIED。
- v1: 停止時フックに「続行させる」機能は持たせない。

## 7. `@` ファイル参照
- 字句解析（`@path` の切り出し）は cli、解決は application の `attach_references`（`WorkspaceRoot::resolve`
  + `FileSystem`、AccessPolicy が効く）。
- ユーザーメッセージに区切り付きテキストブロック（`<file path="…">…</file>`、データと明示）として追加。
  **合成した tool_call / tool_result にしない**。
- 上限: 1 ファイル `max_tool_output_bytes`、合計と件数にも上限。解決できない `@foo` は本文のまま残し stderr で通知。
- v1: テキストのみ（ディレクトリ・画像は対象外 → 画像は §10 の後）。

## 8. バックグラウンド実行
- domain に新ポート `BackgroundCommands`: `start(CommandRequest) -> JobId` / `read(JobId, since, max) ->
  JobOutput { chunk, next, running, exit_code }` / `kill(JobId)`。`JobId` は newtype。`CommandRunner` に混ぜない。
- infrastructure の supervisor は `SandboxedCommandRunner` と同じ封じ込め・proxy、`GroupGuard` を保持、drop で
  全ジョブ kill。上限: 同時ジョブ数、ジョブごとのリングバッファ（末尾保持）、**ジョブの最大寿命**。
- ツール（root 決定: 安全分類を明確に分けるため新ツール）: `start_command`（Destructive）、
  `read_output`（ReadOnly）、`kill_command`（Mutating）。
- 依存の事後検知: `start` 時に manifest の snapshot、終了を観測した最初の `read_output` / `kill` の結果の
  先頭で差分を報告。
- v1: Ctrl-C ではジョブを止めない（ターンだけ）。`/reset` と終了時に全部止める。
- 禁止: 寿命上限なし、出力の無制限保持、背景の変更をチェックポイントに含めたことにすること。

## 9. サブエージェント
- 循環は構築順で解く: 子用レジストリ（spawn なし）→ 子の `AgentLoop` → `SpawnAgentTool(Arc<AgentLoop>)` →
  親レジストリに登録。深さは構造で 1。application に tokio を入れない。
- 中断は future の drop で伝わる。子は毎回 `Interrupt::new()`、`session_id = "{parent}.{call_id}"`。
- v1: **読み取り専用の子のみ**（子レジストリを `is_read_only()` で絞る、`spawn_agent` も ReadOnly）。
  承認は親と**同じゲート**。Ask ポリシーで同時にプロンプトが出ないよう、cli のゲートに async Mutex で直列化。
- イベント: 子は tool 系イベントだけを親のシンクに流す（`render.rs` で字下げ）か `AgentEvent::Nested` —
  実装時に決め、`eval/metrics.rs` と JSON DTO（§5）を追随させる。
- 使用量: `ToolOutcome.usage: Option<TokenUsage>` を足し、dispatcher → ループで親の `session.usage` に積む。
- ユーザー決定: **子の会話は記録に残す**（別 id）。`sessions` 一覧と保持件数の対象からは外す。
- 禁止: 子に別の承認ゲートを渡すこと、application からの `tokio::spawn`、子の履歴を親に混ぜること。

## 10. 画像入力
- domain に `ContentBlock::Image(Image)`（`media_type` は png/jpeg/gif/webp、構築時にサイズ上限を検証）。
  `approx_bytes` は base64 長で数えず名目上の固定コスト。圧縮の書き起こしと `printable` では `[image: N KB png]`。
- 写像: Anthropic は `image` ブロック、OpenAI は data URL。OpenAI 互換は tool ロールに画像不可。
  `ProviderCapabilities.supports_images` を足し、非対応プロバイダには文字の置き換えで送る（400 で落とさない、黙って捨てない）。
- v1: **ユーザーが添付した画像のみ**（`@img.png` と `/image`）、AccessPolicy を通す、1 枚とターンあたりの上限。
  モデルが自分で画像を読むのは v2（`FileSystem::read_bytes` と `ToolResult` の拡張が要る）。
- root 決定: 記録には**本体を上限付きで保存**（1 枚 5 MiB 目安、超えたら添付自体を拒否）。
- **他の機能と並行させない**（ContentBlock の変更は全アダプタと圧縮に波及）。

## 11. Web 検索
- ユーザー決定: **検索 API のポート**。`WebSearcher`（`ports/web.rs`）+ infrastructure に 1 実装
  （Brave Search API、`AGENT_WEB_SEARCH_API_KEY`）。ツール `web_search`、safety は Network、**既定は無効**、
  `AGENT_WEB_SEARCH*` は DENIED、API キーは redactor の対象。結果はツールの結果としてのみ（データ）。
- v1: バックエンド 1 つ、件数上限、結果のリンクは web_fetch 経由でのみ辿れる。テストはモックサーバ。
- Anthropic のサーバツールは使わない（クエリが承認ゲートを通らないため）。

## 12. SDK
- ユーザー決定: **`agent-sdk` crate を作る**。`assemble` の中核（cli 固有の `CliApprovalGate` /
  `TerminalRenderer` / `Notifications` を除いたもの）を `agent-sdk` に移し、cli は SDK の上に乗せる。
- 公開するのは SDK 独自の型（§5 の DTO、`Outcome`、承認のコールバック）と最小限の domain trait
  （`ApprovalGate` / `EventSink` / `Interrupt`）。enum は `#[non_exhaustive]`。domain の型をそのまま安定 API にしない。
- CLAUDE.md の「DI は cli だけ」を「合成ルートは agent-sdk（cli はその上の薄い層）」に更新。
- 着手は §5 の DTO が固まってから。

## 実装順（衝突回避）
1 プランモード → 2 権限ルール → 3 チェックポイント → 4 `/model` `/compact` `/context`（3 の後に直列）。
5 構造化出力は 1〜4 と並行可（cli のみ）。6 フック → 8 背景実行（`exec/` が衝突）。7 `@` 参照は 1・4 の後。
9 サブエージェントは 5 の後。10 画像は単独で。11 Web 検索は並行可。12 SDK は 5 の後。
衝突しやすいファイル: `cli/src/composition.rs`（ほぼ全部）、`cli/src/commands/chat.rs`（1・3・4・7・10）、
`domain/src/ports/events.rs`（8・9）、`message.rs` / `tool.rs`（9・10）、`cli/src/dotenv.rs`。
