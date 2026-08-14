---
title: セッションの永続化と再開
created: 2026-08-14
started: 2026-08-14
completed: 2026-08-14
urgency: low
importance: medium
priority: P2
scope: [domain, infrastructure, cli, docs]
---

# セッションの永続化と再開

## As-is（現状）

`make chat` を終了すると会話は消える。`agent run` も毎回まっさらな状態で始まる。
長い作業を中断すると、再開時にこれまでの経緯・決定事項をゼロから説明し直す必要がある。

## To-be（あるべき姿）

セッションが自動保存され、`agent chat --resume`（または一覧から選択）で
前回の続きから再開できる。中断・再開してもモデルは以前の文脈を覚えている。

## 影響範囲

- **クレート**: `domain`（セッション保存のポート。例 `SessionStore`）、
  `infrastructure`（ファイルベースの実装。`Conversation` は `serde` 対応済みなので
  シリアライズはそのまま使える）、`cli`（`--resume` フラグ、`/sessions` 的な一覧・削除、
  `composition.rs` への結線）
- **不変条件**: なし（§2 との関係で、保存先はワークスペース内 `.agent/` 配下に閉じる。
  `.gitignore` に `/.agent/` は登録済み）
- **設定・ドキュメント**: 保存先・保持数の設定、`.env.example`、README §3
- **利用者への影響**: **会話全文（読んだファイルの内容を含む）がディスクに残る**ようになる。
  この点を README に明記する。既定で有効にするかは要検討

## 達成条件

- [x] chat を終了 → 再起動して直前のセッションを再開でき、モデルが以前の文脈に基づいて
      応答する（`a_session_can_be_resumed_from_its_log`。プロセス再起動そのものは
      テストで再現できないので、メモリ上のセッションを捨ててファイルだけから組み直し、
      次のターンでプロバイダに届く JSON に以前の会話が載ることを確認している）
- [x] セッションの一覧表示と削除ができる（`agent sessions` / `--delete`、
      `listing_is_newest_first_and_says_what_each_session_was_about`、
      `deleting_removes_the_record`）
- [x] 保存先がワークスペース内（`.agent/`）に閉じており、git 管理外である
- [x] 壊れた保存ファイルで起動不能にならない — ただし**読み飛ばしではなく打ち切り**に
      した（下の「決めたこと」を参照。`a_damaged_line_ends_the_replay_instead_of_being_skipped`）
- [x] `make check` が緑（313 passed / 0 failed）

## 優先度の根拠

中断を挟む長い作業では効くが、現状の主用途（短い単発タスク）では困っていない。
回避策（経緯を貼り直す）もある（重要度 medium・緊急度 low）。

## 決めたこと

### 保存用のファイルを別に持たず、会話ログを再生する

計画では `SessionStore` が `Conversation` をシリアライズして保存する想定だった。
先に入れた会話ログ（`conversation-log-append-only`）が既に**刈り込み前の全履歴**を
順番どおり持っているので、それを読み直す形にした。

同じことを言うはずのファイルが 2 つあると、いずれ食い違う。クラッシュが 2 つの書き込みの
間に入るだけで十分で、そうなったときどちらが正しいかを言えるものは何もない。

メモにあった「保存するのは刈り込み前の全履歴か、送信対象の履歴か」への答えでもある。
**全履歴**。再開したセッションは記録の全部を読み込み、予算を超えていれば最初のターンで
通常どおり圧縮される。中断しなかった場合と同じ状態になる。

### 書き手と読み手でポートを分けた

`ConversationLog`（追記のみ）と `SessionStore`（一覧・読み出し・削除）。裏にあるファイルは
同じ 1 つ。ループに必要なのは追記だけで、セッションを削除する権限を渡す理由がない。

### 壊れた行は「読み飛ばし」ではなく「打ち切り」

達成条件には「読み飛ばして」と書いていたが、実装しながら**読み飛ばしは危険**だと分かった。
読めなかった行に `tool_call` があると、対応する `tool_result` が孤児になり、
次のリクエストがまるごとプロバイダに拒否される（不変条件 §4）。

打ち切れば前半が残る。**正しい履歴の前半は正しい履歴**なので、これが安全側。
最後が未応答の `tool_call` で終わる場合は `Conversation::drop_trailing_unanswered_calls`
が落とす（クラッシュがツール呼び出しと結果の間に入った場合に実際に起きる）。

スキーマ版が未知（自分より新しい）の行も同じ扱いで打ち切る。

### 再開に失敗しても起動する

記録がない・壊れている・`--resume` を打ったがまだ何も記録されていない。いずれも
stderr に 1 行出して新規セッションで始まる。習慣で `--resume` を打った人が得るべきなのは
エラーではなく、使えるエージェントと理由の説明。

### 再開すると同じ id を使う

ログが同じファイルに続く。別 id にすると記録が 2 つに割れる。

## 結果

- **domain** — `ports/session_store.rs`（`SessionStore` と `SessionSummary`）、
  `Conversation::drop_trailing_unanswered_calls`
- **infrastructure** — `session/store.rs` の `FileSessionStore`。`session/mod.rs` に
  ログと共有する `file_stem` / `path_for` / `SCHEMA_VERSION` を移した
- **cli** — `agent chat --resume [ID]`（`Resume` enum で 3 状態を表現）、
  `agent sessions [--delete ID]`、`commands/sessions.rs`、`composition.rs` への結線
- **ドキュメント** — README §3（新しい節）/ §7、`management/docs/architecture.md` §3.2.3 と
  ポート表
- **テスト** — 13 本追加、300 → 313
- **やらなかったこと**
  - **`agent run`（単発）は再開に対応していない。** ログは書くが `--resume` は chat のみ。
    単発実行で前回の続きを求める使い方が出てきたら足す
  - **`session.usage`（トークン累計）は再開でリセットされる。** ログに載っていないため。
    `/usage` が再開後に 0 から始まる
  - **一覧からの対話的な選択は作っていない**（issue の「または一覧から選択」）。
    `agent sessions` で id を見て `--resume <ID>` を打つ形。選択 UI が要るなら別 issue
  - **`chat::start` の分岐（再開失敗時のフォールバック）はテストで踏んでいない。**
    `Application` を組み立てるテストの足場が `cli` にないため。分岐が呼ぶ
    `SessionStore` 側のエラー経路はそれぞれテスト済み
  - ログのローテーションと上限は引き続きなし（`conversation-log-append-only` の
    「やらなかったこと」と同じ）

## メモ

- 起票時の 2 点は両方とも解決済み
  - 「保存するのは刈り込み前の全履歴か、送信対象の履歴か」→ **全履歴**。
    ログがそれを持っているので、それを再生する
  - 「スキーマにバージョンを付けておく」→ ログの各行が `v` を持つ。未知の版は打ち切り
- 関連: `done/2026-08-14_conversation-log-append-only.md`（この issue の前提）、
  `done/2026-08-14_recency-weighted-compaction.md`（`Message::seq` は再生時に振り直される）
