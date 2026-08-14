---
title: 会話を追記専用ログに書き出して失わないようにする
created: 2026-08-14
started: 2026-08-14
completed: 2026-08-14
urgency: high
importance: high
priority: P1
scope: [domain, application, infrastructure, cli, docs]
---

# 会話を追記専用ログに書き出して失わないようにする

## As-is（現状）

会話はどこにも残らない。`telemetry.rs` はログを stderr にしか出さず（stdout は回答用に
空けてある）、`Session` はプロセスが終われば消える。

消えるのはプロセス終了時だけではない。**圧縮と刈り込みは実行中に原文を捨てている。**

| 段階 | 何が起きるか |
|---|---|
| 圧縮 | 古いターンが要約 1 通に置換される。要点は残るが原文は戻らない |
| 刈り込み | 予算に収まらない分が**丸ごと削除**される。要約すらない |

ユーザーから見ると、長いセッションで「さっきツールが何を返したか」を後から確認する手段が
ない。ターミナルのスクロールバックが唯一の記録で、それも `AGENT_MAX_TOOL_OUTPUT_BYTES` で
切り詰められた後のものしかない。

## To-be（あるべき姿）

セッション中にやり取りした内容が、そのままファイルに残る。プロセスが落ちても、
圧縮が走っても、刈り込みが起きても、**書かれたものは消えない**。

後から「あのとき何を読んだか」「モデルが何と言ったか」をファイルで辿れる。
機能を切ることもでき、切れば 1 バイトも書かれない。

## 影響範囲

- **クレート**:
  - `domain` … 追記のポート（`ports/conversation_log.rs`）と何もしない実装
  - `application` … `AgentDependencies` に 1 本足し、`AgentLoop` が push のたびに追記する
  - `infrastructure` … ファイルへの追記実装
  - `cli` … `composition.rs` への結線、設定
- **不変条件**: **あり（§1 の作法に従う。§2 に隣接）**
  - **§1 依存は内向きのみ** — ファイル追記は外界。`domain/src/ports/` に trait を足して
    `infrastructure` で実装し `composition.rs` で結線する。規約が定めている手順そのもの
  - **§2 ファイルアクセスは `WorkspacePath` 経由のみ** — このログを書くのは
    *ホストプロセス*であってモデルのツールではないので、サンドボックスの対象外
    （システムプロンプトファイルと同じ立場）。ただし**書き込み先をワークスペース内に置くと、
    モデル自身の `read_file` から読める**。既定値の選択とあわせて明記する
- **設定・ドキュメント**: `.env.example`、`README.md` §4 / §8、
  `management/docs/architecture.md`
- **利用者への影響**: **会話全文（モデルが読んだファイルの中身を含む）がディスクに残る。**
  既定で有効にするなら、この点を README と `.env.example` に目立つ形で書く

## 達成条件

- [x] 1 ターン走らせるとログファイルができ、user / assistant / tool の各メッセージが
      追記されている（`every_message_the_loop_appends_reaches_the_log`、
      `writes_one_line_per_message_and_keeps_appending`）
- [x] **圧縮で畳まれた原文がログには残っている**
      （`the_log_keeps_what_the_compaction_folded_away`）
- [x] **刈り込みで削除された原文がログには残っている**
      （`the_log_keeps_what_the_trimming_deleted`）
- [x] ログの書き込みに失敗してもターンは止まらない
      （`a_broken_log_does_not_stop_the_turn`）
- [x] 1 行 1 レコードで、途中で落ちても壊れた行より前は読める（JSONL）
- [x] レコードにスキーマ版が入っている
      （`every_line_carries_the_session_and_a_schema_version`）
- [x] 機能を無効にすると 1 バイトも書かれない — **構造で保証**。無効は
      `NullConversationLog` を結線することであって分岐ではないので、書く経路が存在しない
- [x] 既定の書き込み先が git 管理外である（`.gitignore` の `/.agent/`）
- [x] `.env.example` と README に、会話全文がディスクに残ることが書かれている
- [x] `make check` が緑（300 passed / 0 failed）

## 優先度の根拠

内容が失われる問題で、失われた後に取り返す手段がない（重要度 `high`）。
かつ、セッション再開（`session-persistence-and-resume`）の前提になる — 何を保存するかを
決める前に、まず失わない場所を用意する必要がある（緊急度 `high`）。score 9 → P1。

## 決めたこと

### `EventSink` を流用せず、専用のポートを足した

一見すると「イベントをファイルに書く `EventSink` の実装」で済みそうだが、**イベントは
記録として使えない**。`AgentEvent::AssistantMessage` は散文だけを運び、隣にあった
`tool_call` は落ちる。ツール結果は `ToolCallFinished` の 1 行要約に縮められている。
表示にはそれが正しく、記録にはそれでは足りない。

`ConversationLog` はメッセージをそのまま運ぶ。役割が違うので、ポートも別にした。

### 書き込みは `fit_history` の前

`AgentLoop` は履歴に push するたびに追記する。次のイテレーションの先頭で `fit_history` が
走り、圧縮と刈り込みはどちらも元に戻せないので、**そこまでに書かれなかったものが
記録から欠けるもの**になる。

追記するメッセージは `Conversation` から取り出している。push する前の `Message` には
通番が付いていないため。

### 失敗は警告して捨てる

圧縮の失敗と同じ扱い。ログが書けないことでセッションが止まるのは、1 行欠けるより悪い。

### セッション id をファイル名に使う前に無害化する

ポートは `&str` を受け取るだけで、id の形を何も約束していない。今日は CLI が UUID を
渡しているが、ユーザー入力から組み立てた id なら `../../.ssh/authorized_keys` が来うる。

ASCII 英数字と `-` `_` 以外は `_` に置換する。**`.` も落とす**ので、`..` は
「起きにくい」ではなく「作れない」。拒否ではなく置換にしたのは、壊れた id が
ログを止められてはいけないため（`a_session_id_cannot_escape_the_directory`）。

### 既定の置き場所はワークスペース内（`.agent/sessions/`）

`.gitignore` に登録済みで、ユーザーが見つけやすい。

**代償として、モデル自身の `read_file` からこのログが読める。**ファイルツールの
サンドボックスはワークスペース内を許すので、`.agent/` も例外ではない。
ワークスペース外に置きたい場合は `AGENT_SESSION_DIR` に絶対パスを指定する
（ログを書くのはホストプロセスなので、外に置くこと自体は問題ない）。

この点は README と `.env.example` に警告として明記した。

## 結果

- **domain** — `ports/conversation_log.rs`（`ConversationLog` と `NullConversationLog`）
- **application** — `AgentDependencies.log`、`AgentLoop::record`。push の 3 箇所
  （ユーザー入力・モデル応答・ツール結果）の直後に呼ぶ
- **infrastructure** — `session/log.rs` の `FileConversationLog`。JSONL、
  `create` + `append`、1 バッチ 1 write。レコードは `{v, session, at, role, content, seq}`
- **cli** — `AGENT_SESSION_LOG` / `AGENT_SESSION_DIR` を結線。`Settings::session_dir()` が
  相対パスをワークスペース基準で解決。`agent doctor` が置き場所を表示する
- **ドキュメント** — `.env.example`、README §4（新しい節）/ §7 / §8、
  `management/docs/architecture.md` §3.2.3 とポート表
- **テスト** — 12 本追加、288 → 300。`make check` 緑
- **やらなかったこと**
  - **ログのローテーションと上限がない。** 長いセッションを繰り返すと
    `.agent/sessions/` は無制限に育つ。別 issue に切り出すのが妥当
  - ログを**読む**側は作っていない（`jq` で読む前提）。一覧・検索が要るなら別 issue
  - 圧縮が「どこからどこまでを畳んだか」はログに記録していない。再開機能が
    要求したときに `v` を上げて足すのがよい
  - 結線そのもの（`AGENT_SESSION_LOG=false` で `NullConversationLog` が選ばれること）は
    テストで踏んでいない。`composition.rs` にテストの足場がないため。設定の解析と
    Null 実装の挙動はそれぞれテスト済み

## メモ
